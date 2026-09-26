//! [`UserAppReadinessReader`] 实现：按 app+stage 只读定位当前物理实例并查询
//! app-cli 业务就绪（rcoder-engine 侧装配，注入 app_manager）。
//!
//! 通道选择（Plan §5.3）：
//! - 非 deploy-host（Compose/集群内）：容器/Pod IP 直连 `:3010`。
//! - deploy-host Docker：优先 Published 端口映射（prod 从 `DeploymentStatus.ports`
//!   读 3010 的 host 映射；dev 从发布注册表解析）；无映射/直连不可用形态走固定
//!   只读命令通道（exec `app-cli readiness --json`）。
//! - deploy-host K8s：固定 exec 命令通道（pods/exec）。
//!
//! 只读纪律：只用 `get_deployment_status`/`find_container`/`exec` 观察原语；
//! 不 ensure/wake/adopt，不触碰 file-server 注册表（agent 自启 owner 可观察）；
//! 观察后按物理身份（IP/容器 ID）复核，换代丢弃结果。

use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::app_state::AppState;
use shared_types::{
    UserAppBusinessReadiness, UserAppProxyReadiness, UserAppReadinessChannel,
    UserAppReadinessObservation, UserAppReadinessPhysical, UserAppReadinessReason,
    UserAppReadinessStatus, UserappStage,
};

/// 直连单次 HTTP 上限（总预算内取小）。
const DIRECT_FETCH_CAP: Duration = Duration::from_secs(4);
/// exec 单次上限（进程 spawn + 容器内 HTTP）。
const EXEC_FETCH_CAP: Duration = Duration::from_secs(6);
/// app-cli 管理 API 端口（容器内恒绑 0.0.0.0）。
const ADMIN_PORT: u16 = shared_types::APP_CLI_ADMIN_PORT;
/// 固定只读查询命令（argv 逐字传递，不经 shell）。
const READINESS_COMMAND: [&str; 5] = [
    "app-cli",
    "readiness",
    "--json",
    "--admin-addr",
    "127.0.0.1:3010",
];

/// Weak 挂接 AppState（与 UserappDevLocator 同款防引用环）。
pub struct UserAppReadinessReaderImpl {
    state: Weak<AppState>,
}

impl UserAppReadinessReaderImpl {
    pub(crate) fn new(state: Weak<AppState>) -> Self {
        Self { state }
    }

    fn state(&self) -> Result<Arc<AppState>, String> {
        self.state
            .upgrade()
            .ok_or_else(|| "app state already dropped".to_string())
    }
}

/// 单次通道尝试的分类结果。
enum FetchOutcome {
    Snapshot(UserAppBusinessReadiness),
    /// 旧运行时：端点 404/405 或 CLI 退出码 3。
    Unsupported,
    /// 传输失败（拒绝/超时/5xx/CLI 退出码 2 等）。
    Unreachable,
}

#[async_trait::async_trait]
impl shared_types::UserAppReadinessReader for UserAppReadinessReaderImpl {
    async fn observe(
        &self,
        app_id: &str,
        stage: UserappStage,
        budget: Duration,
    ) -> Result<UserAppReadinessObservation, String> {
        let state = self.state()?;
        match stage {
            UserappStage::Prod => observe_prod(&state, app_id, budget).await,
            UserappStage::Dev => observe_dev(&state, app_id, budget).await,
        }
    }
}

async fn observe_prod(
    state: &Arc<AppState>,
    app_id: &str,
    budget: Duration,
) -> Result<UserAppReadinessObservation, String> {
    let deploy_host = shared_types::is_deploy_host();
    let deadline = tokio::time::Instant::now() + budget;
    // 观察轮次上限 2：定位 → 查询 → 复核 Pod IP 未换代；换代则**重新定位**
    // 新实例再试一轮（不用旧 IP 重查），仍不稳返回 INSTANCE_CHANGED——
    // 快速换代不会在预算内高频穿透 runtime API。
    for _attempt in 0..2 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let status = state
            .runtime()
            .get_deployment_status(app_id)
            .await
            .map_err(|error| format!("read prod deployment status (app {app_id}): {error}"))?;
        let Some(status) = status else {
            return Ok(UserAppReadinessObservation::NoCompute {
                detail: Some("deployment-missing".into()),
            });
        };
        if status.replicas == 0 {
            return Ok(UserAppReadinessObservation::NoCompute {
                detail: Some("scaled-to-zero".into()),
            });
        }
        let Some(pod_ip) = status.pod_ip.clone().filter(|ip| !ip.is_empty()) else {
            // 副本期望 >0 但尚无 Pod IP（调度中）——不是停止，是启动窗口。
            return Ok(UserAppReadinessObservation::NoCompute {
                detail: Some("no-pod-ip".into()),
            });
        };
        #[cfg(feature = "deploy-host")]
        let published = if deploy_host {
            status
                .ports
                .iter()
                .find(|port| port.port == ADMIN_PORT)
                .and_then(|port| port.external_port)
        } else {
            None
        };
        #[cfg(not(feature = "deploy-host"))]
        let published: Option<u16> = None;

        let (physical, outcome) =
            fetch_once(state, app_id, &pod_ip, published, deploy_host, remaining).await?;
        let recheck = state
            .runtime()
            .get_deployment_status(app_id)
            .await
            .map_err(|error| format!("re-verify prod deployment: {error}"))?;
        let unchanged = recheck
            .as_ref()
            .and_then(|status| status.pod_ip.clone())
            .is_some_and(|current| current == pod_ip);
        if unchanged {
            return Ok(classify_into_observation(physical, outcome));
        }
        tracing::debug!("[READINESS] prod instance changed during observation (app {app_id})");
    }
    Ok(UserAppReadinessObservation::InstanceChanged)
}

async fn observe_dev(
    state: &Arc<AppState>,
    app_id: &str,
    budget: Duration,
) -> Result<UserAppReadinessObservation, String> {
    let deploy_host = shared_types::is_deploy_host();
    let deadline = tokio::time::Instant::now() + budget;
    // 与 prod 同款：最多两轮、每轮重新定位（换代后用新容器 ID/IP 重查）。
    for _attempt in 0..2 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let found = state
            .runtime()
            .find_container(app_id, &shared_types::ServiceType::UserappBuilder)
            .await
            .map_err(|error| format!("find UserappBuilder (app {app_id}): {error}"))?;
        let Some(info) = found else {
            return Ok(UserAppReadinessObservation::NoCompute {
                detail: Some("builder-missing".into()),
            });
        };
        if info.status != container_runtime_api::ContainerRuntimeStatus::Running {
            return Ok(UserAppReadinessObservation::NoCompute {
                detail: Some("builder-not-running".into()),
            });
        }
        let container_ip = info.container_ip.clone();
        let container_id = info.container_id.clone();
        #[cfg(feature = "deploy-host")]
        let published = {
            let container_name = info.container_name.clone();
            if deploy_host {
                shared_types::published::resolve_published_addr(&container_name, ADMIN_PORT)
                    .ok()
                    .map(|addr| addr.port())
            } else {
                None
            }
        };
        #[cfg(not(feature = "deploy-host"))]
        let published: Option<u16> = None;

        let (physical, outcome) = fetch_once(
            state,
            app_id,
            &container_ip,
            published,
            deploy_host,
            remaining,
        )
        .await?;
        let recheck = state
            .runtime()
            .find_container(app_id, &shared_types::ServiceType::UserappBuilder)
            .await
            .map_err(|error| format!("re-verify builder: {error}"))?;
        let unchanged = recheck.is_some_and(|info| info.container_id == container_id);
        if unchanged {
            return Ok(classify_into_observation(physical, outcome));
        }
        tracing::debug!("[READINESS] dev builder changed during observation (app {app_id})");
    }
    Ok(UserAppReadinessObservation::InstanceChanged)
}

/// 单次查询结果 → 观察枚举。
fn classify_into_observation(
    physical: UserAppReadinessPhysical,
    outcome: FetchOutcome,
) -> UserAppReadinessObservation {
    match outcome {
        FetchOutcome::Snapshot(snapshot) => {
            UserAppReadinessObservation::Snapshot { physical, snapshot }
        }
        FetchOutcome::Unsupported => UserAppReadinessObservation::UnsupportedRuntime { physical },
        FetchOutcome::Unreachable => UserAppReadinessObservation::AdminUnreachable { physical },
    }
}

/// 通道选择 + 单次查询（直连优先，exec 兜底——已确定的网络形态不随意回退）。
async fn fetch_once(
    state: &Arc<AppState>,
    app_id: &str,
    container_ip: &str,
    published_port: Option<u16>,
    deploy_host: bool,
    remaining: Duration,
) -> Result<(UserAppReadinessPhysical, FetchOutcome), String> {
    // 1) Published 映射（deploy-host 注册/端口表命中）。
    if let Some(host_port) = published_port {
        let addr = format!("127.0.0.1:{host_port}");
        let outcome = fetch_direct(&addr, remaining.min(DIRECT_FETCH_CAP)).await;
        return Ok((
            UserAppReadinessPhysical {
                instance_id: Some(container_ip.to_string()),
                address: Some(addr),
                channel: UserAppReadinessChannel::Direct,
            },
            outcome,
        ));
    }
    // 2) 容器/Pod IP 直连（非 deploy-host 恒走此处；deploy-host Direct 形态同）。
    #[cfg(feature = "deploy-host")]
    let direct_reachable = !deploy_host || shared_types::deploy_host_reach::is_direct();
    #[cfg(not(feature = "deploy-host"))]
    let direct_reachable = !deploy_host;
    if direct_reachable {
        let addr = format!("{container_ip}:{ADMIN_PORT}");
        let outcome = fetch_direct(&addr, remaining.min(DIRECT_FETCH_CAP)).await;
        return Ok((
            UserAppReadinessPhysical {
                instance_id: Some(container_ip.to_string()),
                address: Some(addr),
                channel: UserAppReadinessChannel::Direct,
            },
            outcome,
        ));
    }
    // 3) 固定只读命令通道（exec；命令 argv 由平台固定，不透传任何用户数据）。
    let command: Vec<String> = READINESS_COMMAND
        .iter()
        .map(|part| part.to_string())
        .collect();
    let exec = tokio::time::timeout(
        remaining.min(EXEC_FETCH_CAP),
        state.runtime().exec(app_id, command),
    )
    .await;
    let physical = UserAppReadinessPhysical {
        instance_id: Some(container_ip.to_string()),
        address: None,
        channel: UserAppReadinessChannel::Exec,
    };
    let exec = match exec {
        Ok(result) => {
            result.map_err(|error| format!("exec readiness command (app {app_id}): {error}"))?
        }
        Err(_) => return Ok((physical, FetchOutcome::Unreachable)),
    };
    Ok((physical, classify_exec(&exec)))
}

/// 直连 GET `/v1/app/readiness` 并按响应分类。
async fn fetch_direct(addr: &str, budget: Duration) -> FetchOutcome {
    let url = format!("http://{addr}/v1/app/readiness");
    let request = async {
        let client = reqwest::Client::builder()
            .connect_timeout(budget)
            .timeout(budget)
            .build()
            .map_err(|error| format!("build client: {error}"))?;
        client
            .get(&url)
            .send()
            .await
            .map_err(|error| format!("GET {url}: {error}"))
    };
    let Ok(response) = tokio::time::timeout(budget, request).await else {
        return FetchOutcome::Unreachable;
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!("[READINESS] direct fetch failed: {error}");
            return FetchOutcome::Unreachable;
        }
    };
    let status = response.status().as_u16();
    match status {
        404 | 405 => return FetchOutcome::Unsupported,
        200..=299 => {}
        _ => return FetchOutcome::Unreachable,
    }
    let Ok(body) = response.text().await else {
        return FetchOutcome::Unreachable;
    };
    match parse_admin_envelope(&body) {
        Ok(Some(snapshot)) => FetchOutcome::Snapshot(snapshot),
        // 200 但信封错误/缺字段：协议错误——不能当 ready，也不是不可达。
        Ok(None) => protocol_invalid_snapshot(),
        Err(_) => protocol_invalid_snapshot(),
    }
}

/// app-cli 信封解析：`{code, message, data}`；成功返回快照。
fn parse_admin_envelope(body: &str) -> anyhow::Result<Option<UserAppBusinessReadiness>> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| anyhow::anyhow!("invalid JSON envelope: {error}"))?;
    let code = value
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if code != "0000" {
        return Ok(None);
    }
    let snapshot = value
        .get("data")
        .cloned()
        .map(serde_json::from_value::<UserAppBusinessReadiness>)
        .transpose()?;
    Ok(snapshot)
}

/// exec 输出分类：退出码 0 = 快照；3 = 旧运行时；其余 = 不可达。
fn classify_exec(exec: &container_runtime_api::ExecResult) -> FetchOutcome {
    match exec.exit_code {
        0 => match serde_json::from_str::<UserAppBusinessReadiness>(exec.stdout.trim()) {
            Ok(snapshot) => FetchOutcome::Snapshot(snapshot),
            // 退出 0 但输出不可解析：协议错误（合成 unknown 快照，不当 ready）。
            Err(error) => {
                tracing::warn!(
                    "readiness exec stdout invalid (exit 0): {error}; stdout head: {:.120}",
                    exec.stdout
                );
                protocol_invalid_snapshot()
            }
        },
        3 => FetchOutcome::Unsupported,
        code => {
            tracing::debug!(
                "readiness exec failed: exit={code}, stderr: {:.200}",
                exec.stderr
            );
            FetchOutcome::Unreachable
        }
    }
}

/// 协议错误 → 合成 unknown 快照（带 ADMIN_PROTOCOL_INVALID；不伪装 stopped/failed）。
fn protocol_invalid_snapshot() -> FetchOutcome {
    FetchOutcome::Snapshot(UserAppBusinessReadiness {
        ready: false,
        status: UserAppReadinessStatus::Unknown,
        reason_code: Some(UserAppReadinessReason::AdminProtocolInvalid),
        checked_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        runtime_instance_id: None,
        serving_release_id: None,
        target_release_id: None,
        operation_id: None,
        observation_revision: 0,
        proxy: UserAppProxyReadiness {
            ready: false,
            status: UserAppReadinessStatus::Unknown,
            reason_code: Some(UserAppReadinessReason::AdminProtocolInvalid),
            error_origin_contract: None,
        },
        services: Vec::new(),
    })
}
