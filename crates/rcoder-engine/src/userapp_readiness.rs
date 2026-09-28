//! 按 app + stage 定位实际实例，查询 app-cli 并复核物理身份。
//! 容器内走管理 IP；宿主机 Docker 走实际发布端口或固定 exec；宿主机
//! K8s 始终 exec 到捕获的 Pod/容器。全程只读，不唤醒、不登记、不申请租约。

use std::sync::{Arc, Weak};
use std::time::Duration;

use container_runtime_api::{
    UserAppDeploymentRuntime, UserAppReadinessInstance, UserAppReadinessTarget,
    UserAppRuntimeReadiness,
};
use tokio::time::Instant;

use crate::app_state::AppState;
use shared_types::{
    UserAppBusinessReadiness, UserAppProxyReadiness, UserAppReadinessChannel,
    UserAppReadinessObservation, UserAppReadinessPhysical, UserAppReadinessReason,
    UserAppReadinessStatus, UserappStage,
};

const DIRECT_FETCH_CAP: Duration = Duration::from_secs(4);
const EXEC_FETCH_CAP: Duration = Duration::from_secs(6);

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

#[derive(Clone, Copy)]
enum Access {
    ContainerNetwork,
    #[cfg_attr(not(feature = "deploy-host"), allow(dead_code))]
    HostDirect,
    HostPublished,
}

impl Access {
    fn current() -> Self {
        if !shared_types::is_deploy_host() {
            return Self::ContainerNetwork;
        }
        #[cfg(feature = "deploy-host")]
        if shared_types::deploy_host_reach::is_direct() {
            return Self::HostDirect;
        }
        Self::HostPublished
    }
}

enum FetchOutcome {
    Snapshot(UserAppBusinessReadiness),
    Unsupported,
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
        observe_runtime(
            state.runtime().as_ref(),
            app_id,
            stage,
            Access::current(),
            budget,
        )
        .await
    }
}

async fn observe_runtime(
    runtime: &dyn UserAppDeploymentRuntime,
    app_id: &str,
    stage: UserappStage,
    access: Access,
    budget: Duration,
) -> Result<UserAppReadinessObservation, String> {
    let deadline = Instant::now() + budget;
    // The same deadline covers discovery, transport and all identity rechecks.
    let observe = async {
        for _ in 0..2 {
            let current = runtime
                .observe_userapp_readiness(app_id, stage)
                .await
                .map_err(|error| format!("Locate {stage:?} readiness (app {app_id}): {error}"))?;
            let target = match current {
                UserAppRuntimeReadiness::NotRunning(state) => {
                    return Ok(UserAppReadinessObservation::NoCompute { state });
                }
                UserAppRuntimeReadiness::Running(target) => target,
            };
            let fetched = fetch_once(runtime, &target, access, deadline).await;
            let recheck = runtime
                .observe_userapp_readiness(app_id, stage)
                .await
                .map_err(|error| format!("Recheck {stage:?} readiness (app {app_id}): {error}"))?;
            if recheck != UserAppRuntimeReadiness::Running(target) {
                continue;
            }
            if let Some((physical, outcome)) = fetched? {
                return Ok(match outcome {
                    FetchOutcome::Snapshot(snapshot) => {
                        UserAppReadinessObservation::Snapshot { physical, snapshot }
                    }
                    FetchOutcome::Unsupported => {
                        UserAppReadinessObservation::UnsupportedRuntime { physical }
                    }
                    FetchOutcome::Unreachable => {
                        UserAppReadinessObservation::AdminUnreachable { physical }
                    }
                });
            }
        }
        Ok(UserAppReadinessObservation::InstanceChanged)
    };
    tokio::time::timeout_at(deadline, observe)
        .await
        .unwrap_or(Ok(UserAppReadinessObservation::TimedOut))
}

fn transport(
    target: &UserAppReadinessTarget,
    access: Access,
) -> (UserAppReadinessChannel, Option<std::net::SocketAddr>) {
    match (&target.instance, access) {
        (_, Access::ContainerNetwork)
        | (UserAppReadinessInstance::Docker { .. }, Access::HostDirect) => {
            (UserAppReadinessChannel::Direct, target.address)
        }
        (UserAppReadinessInstance::Docker { .. }, Access::HostPublished)
            if target.published_address.is_some() =>
        {
            (UserAppReadinessChannel::Direct, target.published_address)
        }
        _ => (UserAppReadinessChannel::Exec, None),
    }
}

async fn fetch_once(
    runtime: &dyn UserAppDeploymentRuntime,
    target: &UserAppReadinessTarget,
    access: Access,
    deadline: Instant,
) -> Result<Option<(UserAppReadinessPhysical, FetchOutcome)>, String> {
    let (channel, address) = transport(target, access);
    let physical = UserAppReadinessPhysical {
        instance_id: Some(target.instance.physical_id().to_owned()),
        address: address.map(|addr| addr.to_string()),
        channel,
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let outcome = match channel {
        UserAppReadinessChannel::Direct => match address {
            Some(addr) => fetch_direct(&addr.to_string(), remaining.min(DIRECT_FETCH_CAP)).await,
            None => FetchOutcome::Unreachable,
        },
        UserAppReadinessChannel::Exec => {
            match tokio::time::timeout(
                remaining.min(EXEC_FETCH_CAP),
                runtime.exec_userapp_readiness(target),
            )
            .await
            {
                Ok(Ok(Some(exec))) => classify_exec(&exec),
                Ok(Ok(None)) => return Ok(None),
                Ok(Err(error)) => {
                    return Err(format!(
                        "Exec readiness for {}: {error}",
                        target.instance.physical_id()
                    ));
                }
                Err(_) => FetchOutcome::Unreachable,
            }
        }
    };
    Ok(Some((physical, outcome)))
}

async fn fetch_direct(addr: &str, budget: Duration) -> FetchOutcome {
    let query = async {
        // 共享探测客户端（不跟随重定向、无全局总超时）；总预算由 per-request
        // .timeout 与外层 tokio timeout 共同收紧。
        let client = crate::http_client::probe_client();
        let response = client
            .get(format!("http://{addr}/v1/app/readiness"))
            .timeout(budget)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        match response.status().as_u16() {
            404 | 405 => return Ok(FetchOutcome::Unsupported),
            200..=299 => {}
            _ => return Ok(FetchOutcome::Unreachable),
        }
        let body = response.text().await.map_err(|error| error.to_string())?;
        Ok::<_, String>(match parse_admin_envelope(&body) {
            Ok(Some(snapshot)) => FetchOutcome::Snapshot(snapshot),
            _ => protocol_invalid_snapshot(),
        })
    };
    match tokio::time::timeout(budget, query).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => {
            tracing::debug!("[READINESS] direct fetch failed: {error}");
            FetchOutcome::Unreachable
        }
        Err(_) => FetchOutcome::Unreachable,
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

#[cfg(test)]
mod tests {
    use super::*;
    use container_runtime_api::{ContainerRuntimeResult, ExecResult};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct Runtime {
        observations: Mutex<VecDeque<UserAppRuntimeReadiness>>,
        commands: Mutex<Vec<UserAppReadinessTarget>>,
        pending: bool,
    }
    #[async_trait::async_trait]
    impl UserAppDeploymentRuntime for Runtime {
        async fn observe_userapp_readiness(
            &self,
            app_id: &str,
            stage: UserappStage,
        ) -> ContainerRuntimeResult<UserAppRuntimeReadiness> {
            if self.pending {
                return std::future::pending().await;
            }
            let mut observations = self.observations.lock().unwrap();
            let value = if observations.len() > 1 {
                observations.pop_front().unwrap()
            } else {
                observations.front().unwrap().clone()
            };
            if let UserAppRuntimeReadiness::Running(target) = &value {
                assert_eq!(target.app_id, app_id);
                assert_eq!(target.stage, stage);
            }
            Ok(value)
        }
        async fn exec_userapp_readiness(
            &self,
            target: &UserAppReadinessTarget,
        ) -> ContainerRuntimeResult<Option<ExecResult>> {
            self.commands.lock().unwrap().push(target.clone());
            let FetchOutcome::Snapshot(mut snapshot) = protocol_invalid_snapshot() else {
                unreachable!()
            };
            snapshot.runtime_instance_id = Some(target.instance.physical_id().into());
            Ok(Some(ExecResult {
                exit_code: 0,
                stdout: serde_json::to_string(&snapshot).unwrap(),
                stderr: String::new(),
            }))
        }
    }
    fn docker_target(stage: UserappStage, id: &str) -> UserAppReadinessTarget {
        UserAppReadinessTarget {
            app_id: "194".into(),
            stage,
            instance: UserAppReadinessInstance::Docker {
                container_id: id.into(),
                started_at: Some("first-start".into()),
            },
            address: None,
            published_address: None,
        }
    }
    fn runtime(targets: Vec<UserAppReadinessTarget>) -> Runtime {
        Runtime {
            observations: Mutex::new(
                targets
                    .into_iter()
                    .map(|target| UserAppRuntimeReadiness::Running(Box::new(target)))
                    .collect(),
            ),
            commands: Mutex::default(),
            pending: false,
        }
    }

    #[tokio::test]
    async fn host_exec_preserves_dev_and_prod_targets_without_pod_ip() {
        for stage in [UserappStage::Dev, UserappStage::Prod] {
            let target = docker_target(stage, stage.as_str());
            let runtime = runtime(vec![target.clone()]);
            let observed = observe_runtime(
                &runtime,
                "194",
                stage,
                Access::HostPublished,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert!(matches!(
                observed,
                UserAppReadinessObservation::Snapshot { .. }
            ));
            assert_eq!(*runtime.commands.lock().unwrap(), vec![target]);
        }
    }

    #[tokio::test]
    async fn rereads_instance_when_same_ip_is_reused_or_container_restarted() {
        let old = docker_target(UserappStage::Dev, "same-container");
        let mut new = old.clone();
        if let UserAppReadinessInstance::Docker { started_at, .. } = &mut new.instance {
            *started_at = Some("second-start".into());
        }
        let runtime = runtime(vec![old.clone(), new.clone(), new.clone()]);
        let observed = observe_runtime(
            &runtime,
            "194",
            UserappStage::Dev,
            Access::HostPublished,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert!(matches!(
            observed,
            UserAppReadinessObservation::Snapshot { .. }
        ));
        assert_eq!(*runtime.commands.lock().unwrap(), vec![old, new]);
    }

    #[test]
    fn host_k8s_always_execs_exact_container_even_in_direct_mode() {
        let mut target = docker_target(UserappStage::Dev, "unused");
        target.instance = UserAppReadinessInstance::Kubernetes {
            namespace: "test".into(),
            pod_name: "builder-0".into(),
            pod_uid: "uid".into(),
            container_name: "agent".into(),
            container_id: Some("runtime-id".into()),
            owner_uid: "sts-uid".into(),
        };
        target.address = Some("10.0.0.3:3010".parse().unwrap());
        target.published_address = Some("127.0.0.1:33010".parse().unwrap());
        for access in [Access::HostDirect, Access::HostPublished] {
            assert_eq!(
                transport(&target, access),
                (UserAppReadinessChannel::Exec, None)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn total_deadline_includes_stalled_runtime_discovery() {
        let runtime = Runtime {
            pending: true,
            ..runtime(vec![])
        };
        let started = Instant::now();
        let observed = observe_runtime(
            &runtime,
            "194",
            UserappStage::Dev,
            Access::HostPublished,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert!(matches!(observed, UserAppReadinessObservation::TimedOut));
        assert_eq!(started.elapsed(), Duration::from_secs(1));
        assert!(runtime.commands.lock().unwrap().is_empty());
    }
}
