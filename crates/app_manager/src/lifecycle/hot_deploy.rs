//! 热部署（deploy_mode=hot）：调容器内 app-cli server 的 `/v1/deploy` 端点
//! 原地换应用——不换 Pod、PG/ttyd/dbx 不断连，仅应用服务切换。
//!
//! 定位是**优化路径**：一切前置不满足（app 不存在/不在跑/容器未配
//! `APP_CLI_DEPLOY_TOKEN`/明确缺少协议能力）自动回退换 Pod 权威链；
//! 受理后失败（部署失败/超时）保留现场报错（旧制品 URL 重发即回滚，对齐
//! activate 失败语义）。
//!
//! 等待边界与冷部署链（[`crate::lifecycle`] wait_deploy_stage，部署段完成即
//! 返回）不同：热部署须等匹配操作 `Running`、持久化确认和 `/ready` 业务就绪，
//! 再复核操作身份。准备期间旧服务继续运行；激活会停止旧进程，允许短暂不可用。
//!
//! 成功后把部署三元组经 `update_env_configmap` 收敛进 ConfigMap（K8s-only，
//! 不触碰 Deployment → 无 Recreate）：Pod 重建时 server 按 env 恢复最新版本，
//! 热部署的换代效果不因重建丢失。

use std::{sync::Arc, time::Duration};

use crate::config::AppAccessMode;
use container_runtime_api::UserAppRuntime;

use shared_types::AppCliDeployPhase;
use tracing::{info, warn};

use crate::error::AppOperationError;
use crate::error::AppResult;
use crate::models::AppRuntimeInfo;
use crate::service::AppService;

/// 热部署受理/轮询端口（app-cli 管理 API 常量对齐）。
const APP_CLI_ADMIN_PORT: u16 = shared_types::APP_CLI_ADMIN_PORT;
/// 轮询间隔/预算（对齐冷部署链部署段等待的量级）。
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const HOT_DEPLOY_BUDGET: Duration = Duration::from_secs(300);
const HOT_RECONCILIATION_BUDGET: Duration = Duration::from_secs(30 * 60);

impl AppService {
    /// 尝试热部署。`Ok(Some(()))` = 完成（调用方跳过换 Pod 链）；`Ok(None)` = 前置不满足，回退换 Pod；`Err` = 受理后失败（现场
    /// 保留）。409（已有部署在进行）如实上抛冲突。
    pub(crate) async fn try_deploy_via_container_api(
        &self,
        app_id: &str,
        url: &str,
        release_id: &str,
        sha256: &str,
        requested_business_env: Option<&std::collections::HashMap<String, String>>,
    ) -> AppResult<Option<()>> {
        let operation = self.try_acquire_process_release_lock(app_id).await?;
        // 前置：app 存在且 Running 且有可路由 IP
        let app: AppRuntimeInfo = match self.get_app(app_id).await {
            Ok(app) => app,
            Err(AppOperationError::NotFound(_)) => {
                info!(
                    "[APP] hot deploy fallback: app {app_id} not found (first deploy → pod path)"
                );
                operation.finish().await?;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        // 前置：容器配置了部署令牌（未配置 = 端点禁用 = 镜像未升级到 server 形态）
        let env_snapshot = self
            .runtime
            .app_env_snapshot(app_id)
            .await
            .map_err(|e| AppOperationError::Backend(format!("read hot deployment env: {e}")))?;
        if let Some(requested) = requested_business_env {
            validate_requested_hot_env(requested, &env_snapshot.env)?;
        }
        let Some(ip) = app
            .health
            .instance
            .as_ref()
            .map(|instance| instance.ip.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            warn!("[APP] hot deploy fallback: app {app_id} has no ready runtime IP");
            operation.finish().await?;
            return Ok(None);
        };

        let token = env_snapshot
            .env
            .get("APP_CLI_DEPLOY_TOKEN")
            .cloned()
            .filter(|token| !token.trim().is_empty());
        let Some(token) = token else {
            warn!(
                "[APP] hot deploy fallback: APP_CLI_DEPLOY_TOKEN not set on app {app_id} \
                 (server-form image required)"
            );
            operation.finish().await?;
            return Ok(None);
        };

        // 受理
        let base = format!("http://{ip}:{APP_CLI_ADMIN_PORT}");
        let client = admin_client()?;
        let capability = client
            .get(format!("{base}/v1/deploy/status"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| {
                AppOperationError::Backend(format!("hot deployment capability probe failed: {e}"))
            })?;
        if capability.status().as_u16() == 404 {
            operation.finish().await?;
            return Ok(None);
        }
        let capability: serde_json::Value = capability
            .error_for_status()
            .map_err(|e| {
                AppOperationError::Backend(format!("hot deployment capability status: {e}"))
            })?
            .json()
            .await
            .map_err(|e| {
                AppOperationError::Backend(format!("hot deployment capability body: {e}"))
            })?;
        if !supports_hot_protocol(&capability) {
            operation.finish().await?;
            return Ok(None);
        }
        let generation_id = env_snapshot
            .env
            .get(shared_types::APP_DEPLOY_GENERATION_ID)
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .ok_or_else(|| {
                AppOperationError::Backend("hot deployment generation missing".into())
            })?;
        let task = HotDeploymentTask {
            runtime: self.runtime.clone(),
            access_mode: self.config.access_mode,
            app_id: app_id.to_owned(),
            url: url.to_owned(),
            release_id: release_id.to_owned(),
            sha256: sha256.to_owned(),
            base,
            token,
            env_snapshot,
            generation_id,
            operation_id: uuid::Uuid::new_v4().simple().to_string(),
            operation,
        };
        // Once the request can be accepted, cancellation of the HTTP caller must
        // not abandon status observation or configuration convergence.
        await_hot_coordinator(tokio::spawn(task.run()), HOT_DEPLOY_BUDGET).await
    }
}

/// Dropping the JoinHandle detaches the owned coordinator; it does not cancel
/// the accepted deployment or its conditional configuration commit.
async fn await_hot_coordinator(
    task: tokio::task::JoinHandle<AppResult<Option<()>>>,
    budget: Duration,
) -> AppResult<Option<()>> {
    tokio::time::timeout(budget, task).await.map_err(|_| {
        AppOperationError::Backend("hot deployment wait timed out; coordinator continues; inspect operation status before retry".into())
    })?.map_err(|error| {
        AppOperationError::Backend(format!("hot deployment coordinator failed: {error}"))
    })?
}

struct HotDeploymentTask {
    runtime: Arc<dyn UserAppRuntime>,
    access_mode: AppAccessMode,
    app_id: String,
    url: String,
    release_id: String,
    sha256: String,
    base: String,
    token: String,
    env_snapshot: shared_types::AppEnvSnapshot,
    generation_id: String,
    operation_id: String,
    operation: crate::service::AppOperationGuard,
}

impl HotDeploymentTask {
    async fn run(self) -> AppResult<Option<()>> {
        let result = self.execute().await;
        if let Err(error) = &result {
            warn!(app_id = %self.app_id, operation_id = %self.operation_id, %error,
                "Hot deployment coordinator stopped with an error");
        }
        finish_hot_operation(self.operation, result).await
    }

    async fn execute(&self) -> AppResult<Option<()>> {
        let Self {
            runtime,
            access_mode,
            app_id,
            url,
            release_id,
            sha256,
            base,
            token,
            env_snapshot,
            generation_id,
            operation_id,
            operation,
        } = self;
        let access_mode = *access_mode;
        let body = serde_json::json!({
            "operation_id": operation_id,
            "deployment_generation_id": generation_id,
            "url": url,
            "release_id": release_id,
            "sha256": if sha256.is_empty() { None } else { Some(sha256) },
        });
        operation.mark_mutating()?;
        let resp = admin_client()?
            .post(format!("{base}/v1/deploy"))
            .timeout(Duration::from_secs(30))
            .header("X-Deploy-Token", token.as_str())
            .json(&body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().as_u16() == 202 => {}
            Ok(r) if r.status().as_u16() == 409 => {
                operation.mark_rejected_before_mutation();
                // 透传容器侧错误体（含 "deploy in progress (phase=...)" 真实相位）
                let detail = r.text().await.map_err(|error| {
                    AppOperationError::Backend(format!("read hot deployment rejection: {error}"))
                })?;
                return Err(AppOperationError::Conflict(format!(
                    "hot deploy rejected by container: {detail}"
                )));
            }
            Ok(r) => {
                if matches!(r.status().as_u16(), 400 | 401 | 403 | 404 | 405 | 422) {
                    operation.mark_rejected_before_mutation();
                }
                return Err(AppOperationError::Backend(format!(
                    "hot deployment request rejected: HTTP {}",
                    r.status()
                )));
            }
            Err(e) => {
                return Err(AppOperationError::Backend(format!(
                    "hot deployment acceptance uncertain; inspect operation {operation_id} before retry: {e}"
                )));
            }
        }

        info!("[APP] hot deploy accepted: app_id={app_id}, release_id={release_id}");

        // Completion requires the matched operation and independent business readiness.
        let client = admin_client()?;
        let deadline = tokio::time::Instant::now() + HOT_RECONCILIATION_BUDGET;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(AppOperationError::Backend(
                    "hot deployment reconciliation deadline exceeded; ownership retained; inspect deployment status before retry"
                        .to_string(),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            let probe = self.read_ready_operation(&client).await?;
            // 穷尽 match：新增 AppCliDeployPhase 变体时编译错，强制同步本判据
            match probe.as_ref().map(|p| p.phase) {
                Some(AppCliDeployPhase::Running) => break,
                Some(AppCliDeployPhase::Failed) => {
                    let error = format!(
                        "hot deploy operation {operation_id} failed: {}",
                        probe
                            .as_ref()
                            .and_then(|p| p.error.as_deref())
                            .unwrap_or("see app-cli logs for recovery outcome")
                    );
                    return Err(AppOperationError::Backend(error));
                }
                // deploying/orchestrating（换应用进行中）、idle（受理竞态窗口）、
                // None（不可达/未知相位——旧镜像）→ 继续等
                Some(
                    AppCliDeployPhase::Idle
                    | AppCliDeployPhase::Deploying
                    | AppCliDeployPhase::Orchestrating,
                )
                | None => {}
            }
        }

        // 收敛 env 三元组进 ConfigMap（不触发 Recreate）：Pod 重建恢复最新版本
        let mut env = env_snapshot.env.clone();
        crate::release_flow::identity::strip_release_identity(&mut env);
        env.insert("APP_DEPLOY_URL".into(), url.clone());
        env.insert("APP_RELEASE_ID".into(), release_id.clone());
        env.insert("APP_DEPLOY_SHA256".into(), sha256.clone());
        env.insert(
            shared_types::APP_DEPLOY_OPERATION_ID.into(),
            operation_id.clone(),
        );
        converge_deploy_env_after_hot(
            runtime.as_ref(),
            access_mode,
            app_id,
            env_snapshot,
            &env,
            operation,
        )
        .await?;
        info!("[APP] hot deploy done (pod kept): app_id={app_id}, release_id={release_id}");
        Ok(Some(()))
    }
    async fn read_ready_operation(
        &self,
        client: &reqwest::Client,
    ) -> AppResult<Option<shared_types::AppDeploymentOperation>> {
        let probe = self.read_operation(client).await?;
        if probe
            .as_ref()
            .is_some_and(|operation| operation.phase == AppCliDeployPhase::Running)
        {
            // Readiness has no operation identity: fence it with status reads on
            // both sides so an intervening deployment cannot confirm this operation.
            if self.application_ready(client).await? {
                return self.read_operation(client).await;
            }
            return Ok(None);
        }
        Ok(probe)
    }

    async fn read_operation(
        &self,
        client: &reqwest::Client,
    ) -> AppResult<Option<shared_types::AppDeploymentOperation>> {
        let response = client
            .get(format!("{}/v1/deploy/status", self.base))
            .timeout(Duration::from_secs(10))
            .header("X-Deploy-Token", self.token.as_str())
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                let document = response
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|error| {
                        AppOperationError::Backend(format!(
                            "invalid hot deployment status: {error}"
                        ))
                    })?;
                Ok(observe_hot_operation(
                    &document,
                    &self.operation_id,
                    &self.release_id,
                    &self.generation_id,
                    &self.operation,
                ))
            }
            Ok(response) if response.status().is_client_error() => Err(AppOperationError::Backend(
                format!("hot deployment status rejected: HTTP {}", response.status()),
            )),
            _ => Ok(None),
        }
    }

    async fn application_ready(&self, client: &reqwest::Client) -> AppResult<bool> {
        let response = client
            .get(format!("{}/ready", self.base))
            .timeout(Duration::from_secs(10))
            .header("X-Deploy-Token", self.token.as_str())
            .send()
            .await;
        match response {
            Ok(response) if response.status() == reqwest::StatusCode::OK => {
                let document = response
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|error| {
                        AppOperationError::Backend(format!(
                            "invalid hot deployment readiness: {error}"
                        ))
                    })?;
                if document.get("status").and_then(serde_json::Value::as_str) != Some("ready") {
                    return Err(AppOperationError::Backend(
                        "invalid successful hot deployment readiness response".into(),
                    ));
                }
                Ok(document.get("phase").and_then(serde_json::Value::as_str) == Some("running"))
            }
            Ok(response) if response.status().is_client_error() => {
                Err(AppOperationError::Backend(format!(
                    "hot deployment readiness rejected: HTTP {}",
                    response.status()
                )))
            }
            _ => Ok(false),
        }
    }
}

/// 热部署成功后的 env 收敛：live env 读回 → 剥离历史三元组 → 注入本次
/// 三元组 → 条件写入 ConfigMap。任何冲突或失败均显式报告，禁止伪装完全成功。
async fn converge_deploy_env_after_hot(
    runtime: &dyn UserAppRuntime,
    access_mode: AppAccessMode,
    app_id: &str,
    snapshot: &shared_types::AppEnvSnapshot,
    env: &std::collections::HashMap<String, String>,
    operation: &crate::service::AppOperationGuard,
) -> AppResult<()> {
    // Docker environment is immutable. The matched protocol-4 Running status
    // is accepted only after durable journal readback, checked by the observer.
    if access_mode == AppAccessMode::Docker {
        return Ok(());
    }
    runtime
        .update_env_configmap_if_version(app_id, env, snapshot)
        .await
        .map_err(|e| {
            if matches!(e, container_runtime_api::ContainerRuntimeError::Conflict(_)) {
                operation.mark_completed();
            }
            AppOperationError::Backend(format!(
                "application activated but deployment env convergence failed: {e}"
            ))
        })?;
    Ok(())
}
fn validate_requested_hot_env(
    requested: &std::collections::HashMap<String, String>,
    current: &std::collections::HashMap<String, String>,
) -> AppResult<()> {
    use crate::release_flow::identity::business_env;
    if business_env(requested.clone()) != business_env(current.clone()) {
        return Err(AppOperationError::Validation(
            "hot deployment cannot change business environment; use pod mode".into(),
        ));
    }
    Ok(())
}

fn admin_client() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|error| {
            AppOperationError::Backend(format!("create hot deployment admin client: {error}"))
        })
}

fn supports_hot_protocol(document: &serde_json::Value) -> bool {
    document
        .pointer("/data/protocol_version")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|version| {
            version == u64::from(shared_types::app_cli_deploy::APP_CLI_UNIFIED_DEPLOY_PROTOCOL)
        })
}

async fn finish_hot_operation(
    operation: crate::service::AppOperationGuard,
    result: AppResult<Option<()>>,
) -> AppResult<Option<()>> {
    if result.is_ok() || !operation.has_unfinished_mutation() {
        operation.finish().await?;
    }
    result
}

/// Only a matched, durable and quiescent failure completes ownership. A failed
/// operation can coexist with unfinished shutdown, so its phase alone is insufficient.
fn observe_hot_operation(
    document: &serde_json::Value,
    operation_id: &str,
    release_id: &str,
    generation_id: &str,
    guard: &crate::service::AppOperationGuard,
) -> Option<shared_types::AppDeploymentOperation> {
    let operation: shared_types::AppDeploymentOperation =
        serde_json::from_value(document.pointer("/data/operation")?.clone()).ok()?;
    if !supports_hot_protocol(document)
        || operation.operation_id != operation_id
        || operation.request_release_id != release_id
        || operation.deployment_generation_id != generation_id
    {
        return None;
    }
    if operation.phase == AppCliDeployPhase::Running
        && (operation.deploy_stage != shared_types::AppDeploymentStage::Succeeded
            || !operation.persisted)
    {
        return None;
    }
    if operation.phase == AppCliDeployPhase::Failed {
        if !operation.persisted {
            return None;
        }
        if document
            .pointer("/data/protocol_version")
            .and_then(serde_json::Value::as_u64)?
            < u64::from(shared_types::app_cli_deploy::APP_CLI_QUIESCENT_DEPLOY_PROTOCOL)
        {
            return None;
        }
        let server_phase: AppCliDeployPhase =
            serde_json::from_value(document.pointer("/data/phase")?.clone()).ok()?;
        let recovery_complete = operation
            .recovery
            .as_ref()
            .is_none_or(|recovery| matches!(recovery.status.as_str(), "restored" | "failed"));
        if !matches!(
            server_phase,
            AppCliDeployPhase::Running | AppCliDeployPhase::Failed
        ) || !recovery_complete
        {
            return None;
        }
        guard.mark_completed();
    }
    Some(operation)
}

#[cfg(test)]
mod tests {
    use crate::models::StartAppRequest;
    use crate::test_support::{MockRuntime, test_service};
    use std::sync::Arc;

    #[test]
    fn hot_capability_accepts_operation_identity_and_quiescent_protocols() {
        for (version, supported) in [(0, false), (1, false), (2, false), (3, false), (4, true)] {
            assert_eq!(
                super::supports_hot_protocol(
                    &serde_json::json!({"data":{"protocol_version":version}})
                ),
                supported
            );
        }
        assert!(!super::supports_hot_protocol(
            &serde_json::json!({"data":{}})
        ));
    }

    /// app 不存在 → 回退换 Pod（None），不触发任何容器调用。
    #[tokio::test]
    async fn hot_deploy_falls_back_when_app_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        let svc = test_service(tmp.path(), runtime.clone());

        let outcome = svc
            .try_deploy_via_container_api("app-nope", "http://x/p.zip", "rel-1", "", None)
            .await
            .expect("fallback must not error");
        assert!(outcome.is_none(), "missing app must fall back to pod path");
    }

    /// app 在跑但未配 APP_CLI_DEPLOY_TOKEN → 回退（server 形态镜像未就位）。
    #[tokio::test]
    async fn hot_deploy_falls_back_without_token() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        runtime.deployments.insert(
            "app-live".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "app-live".into(),
                replicas: 1,
                ready_replicas: 1,
                phase: "Running".into(),
                ..Default::default()
            },
        );
        let svc = test_service(tmp.path(), runtime);

        let outcome = svc
            .try_deploy_via_container_api("app-live", "http://x/p.zip", "rel-1", "", None)
            .await
            .expect("fallback must not error");
        assert!(
            outcome.is_none(),
            "missing token must fall back to pod path"
        );
    }

    // /v1/deploy/status phase 解析（信封/裸顶层双兼容 + 未知相位容错）的
    // 用例已随 `parse_deploy_status` 迁至 deploy_wait.rs（`parse_deploy_status_shapes`）。

    /// deploy_mode wire：默认缺省（pod）+ hot 受理。
    #[test]
    fn deploy_mode_wire_default_and_hot() {
        let req: StartAppRequest =
            serde_json::from_str(r#"{"user_id":"u1","url":"http://x/p.zip"}"#).expect("parse");
        assert!(req.deploy_mode.is_none(), "default must be absent (= pod)");

        let req: StartAppRequest =
            serde_json::from_str(r#"{"user_id":"u1","url":"http://x/p.zip","deploy_mode":"hot"}"#)
                .expect("parse");
        assert_eq!(req.deploy_mode, Some(crate::models::DeployMode::Hot));

        // 非法值拒绝（枚举校验）
        assert!(
            serde_json::from_str::<StartAppRequest>(
                r#"{"url":"http://x/p.zip","deploy_mode":"fast"}"#
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn confirmed_hot_failure_releases_ownership_while_pending_and_foreign_status_do_not() {
        for kubernetes in [false, true] {
            for (protocol, operation_id, phase, recovery, persisted, stage, release) in [
                (4, "current", "failed", None, true, "failed", true),
                (4, "current", "failed", None, false, "failed", false),
                (4, "current", "failed", None, true, "succeeded", true),
                (4, "current", "failed", None, false, "succeeded", false),
                (
                    4,
                    "current",
                    "running",
                    Some("restored"),
                    true,
                    "failed",
                    true,
                ),
                (4, "current", "failed", Some("failed"), true, "failed", true),
                (
                    4,
                    "current",
                    "orchestrating",
                    Some("pending"),
                    true,
                    "failed",
                    false,
                ),
                (
                    4,
                    "current",
                    "failed",
                    Some("unknown"),
                    true,
                    "failed",
                    false,
                ),
                (4, "old", "failed", None, true, "failed", false),
                (2, "current", "failed", None, true, "failed", false),
            ] {
                let root = tempfile::tempdir().expect("directory");
                let runtime = Arc::new(MockRuntime::default());
                let mut service = test_service(root.path(), runtime.clone());
                if kubernetes {
                    service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
                }
                let guard = service
                    .acquire_process_release_lock("hot-terminal")
                    .await
                    .expect("lease");
                guard.mark_mutating().expect("accepted mutation");
                let document = serde_json::json!({"data": {"protocol_version":protocol,"phase":phase,"operation": {
                    "operation_id":operation_id,"request_release_id":"release-b", "artifact_release_id":null,
                    "deployment_generation_id":"generation", "persisted":persisted, "deploy_stage":stage,
                    "phase":"failed","error":"download 404", "recovery": recovery.map(|status| serde_json::json!({
                        "status":status,"error":null,"database_migrations_reversed":false
                    }))
                }}});
                let observed = super::observe_hot_operation(
                    &document,
                    "current",
                    "release-b",
                    "generation",
                    &guard,
                );
                assert_eq!(observed.is_some(), release);
                let result = super::finish_hot_operation(
                    guard,
                    Err(crate::error::AppOperationError::Backend("failed".into())),
                )
                .await;
                assert!(result.is_err());
                let next = service
                    .try_acquire_process_release_lock("hot-terminal")
                    .await;
                assert_eq!(
                    next.is_ok(),
                    release,
                    "k8s={kubernetes}, phase={phase}, recovery={recovery:?}, persisted={persisted}, stage={stage}"
                );
                if let Ok(next) = next {
                    next.finish().await.expect("release next");
                }
            }
        }
    }

    #[tokio::test]
    async fn hot_env_cas_rejection_releases_but_uncertain_write_retains_ownership() {
        for failure in [1, 2] {
            let root = tempfile::tempdir().expect("directory");
            let runtime = Arc::new(MockRuntime::default());
            runtime
                .env_commit_failure
                .store(failure, std::sync::atomic::Ordering::SeqCst);
            let mut service = test_service(root.path(), runtime);
            service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
            let guard = service
                .acquire_process_release_lock("hot-env")
                .await
                .expect("lease");
            guard.mark_mutating().expect("accepted mutation");
            let result = super::converge_deploy_env_after_hot(
                service.runtime.as_ref(),
                service.config.access_mode,
                "hot-env",
                &Default::default(),
                &Default::default(),
                &guard,
            )
            .await;
            assert!(result.is_err());
            assert!(
                super::finish_hot_operation(guard, result.map(|()| Some(())))
                    .await
                    .is_err()
            );
            let next = service.try_acquire_process_release_lock("hot-env").await;
            assert_eq!(next.is_ok(), failure == 1);
            if let Ok(next) = next {
                next.finish().await.expect("release");
            }
        }
    }
    #[test]
    fn hot_env_snapshot_is_rechecked_before_fallback_or_admission() {
        use std::collections::HashMap;
        let request = HashMap::from([("BUSINESS".into(), "old".into())]);
        let mut current = request.clone();
        current.insert("APP_DEPLOY_OPERATION_ID".into(), "platform".into());
        assert!(super::validate_requested_hot_env(&request, &current).is_ok());
        current.insert("BUSINESS".into(), "newer-concurrent-value".into());
        assert!(super::validate_requested_hot_env(&request, &current).is_err());
    }

    #[tokio::test]
    async fn running_requires_exact_generation_successful_stage_and_persisted_receipt() {
        let root = tempfile::tempdir().expect("directory");
        let service = test_service(root.path(), Arc::new(MockRuntime::default()));
        let guard = service
            .acquire_process_release_lock("hot-identity")
            .await
            .expect("lease");
        for (generation, stage, persisted, accepted) in [
            ("generation-b", "succeeded", true, true),
            ("generation-a", "succeeded", true, false),
            ("generation-b", "pending", true, false),
            ("generation-b", "failed", true, false),
            ("generation-b", "succeeded", false, false),
        ] {
            let document = serde_json::json!({"data":{
                "protocol_version":4,"phase":"running","operation":{
                    "operation_id":"operation-b","request_release_id":"request-b",
                    "artifact_release_id":"artifact-from-manifest",
                    "deployment_generation_id":generation,"deploy_stage":stage,
                    "persisted":persisted,"phase":"running","error":null
                }
            }});
            assert_eq!(
                super::observe_hot_operation(
                    &document,
                    "operation-b",
                    "request-b",
                    "generation-b",
                    &guard
                )
                .is_some(),
                accepted
            );
        }
        guard.finish().await.expect("finish");
    }

    #[tokio::test]
    async fn accepted_hot_coordinator_survives_cancelled_caller() {
        use axum::{
            Json, Router,
            routing::{get, post},
        };
        for timeout in [false, true] {
            let root = tempfile::tempdir().expect("directory");
            let runtime = Arc::new(MockRuntime::default());
            // If the Docker path accidentally calls mutable-env convergence, it fails.
            runtime
                .env_commit_failure
                .store(2, std::sync::atomic::Ordering::SeqCst);
            let service = test_service(root.path(), runtime.clone());
            let guard = service
                .acquire_process_release_lock("hot-cancel")
                .await
                .expect("lease");
            let accepted = Arc::new(tokio::sync::Notify::new());
            let post_accepted = accepted.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let address = listener.local_addr().expect("address");
            let router = Router::new()
            .route("/v1/deploy", post(move |Json(body): Json<serde_json::Value>| {
                let post_accepted = post_accepted.clone();
                async move {
                assert_eq!(body["deployment_generation_id"], "generation");
                post_accepted.notify_one();
                axum::http::StatusCode::ACCEPTED
                }
            }))
            .route("/ready", get(|| async { Json(serde_json::json!({"status":"ready","phase":"running"})) }))
            .route("/v1/deploy/status", get(|| async {
                Json(serde_json::json!({"data":{
                    "protocol_version":4,"phase":"running","operation":{
                        "operation_id":"operation","request_release_id":"request",
                        "deployment_generation_id":"generation","artifact_release_id":"artifact",
                        "deploy_stage":"succeeded","persisted":true,"phase":"running","error":null
                    }
                }}))
            }));
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.expect("serve");
            });
            let task = super::HotDeploymentTask {
                runtime,
                access_mode: crate::config::AppAccessMode::Docker,
                app_id: "hot-cancel".into(),
                url: "http://artifact".into(),
                release_id: "request".into(),
                sha256: String::new(),
                base: format!("http://{address}"),
                token: "token".into(),
                env_snapshot: Default::default(),
                generation_id: "generation".into(),
                operation_id: "operation".into(),
                operation: guard,
            };
            let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
            let caller = tokio::spawn(async move {
                let worker = tokio::spawn(async move {
                    let result = task.run().await;
                    drop(
                        done_tx.send(
                            result
                                .as_ref()
                                .map(|value| *value)
                                .map_err(ToString::to_string),
                        ),
                    );
                    result
                });
                super::await_hot_coordinator(worker, std::time::Duration::from_millis(20)).await
            });
            tokio::select! {
                outcome = &mut done_rx => panic!("coordinator finished before admission: {outcome:?}"),
                admitted = tokio::time::timeout(std::time::Duration::from_secs(10), accepted.notified()) => {
                    admitted.expect("admission deadline");
                }
            }
            if timeout {
                assert!(
                    caller.await.expect("caller").is_err(),
                    "HTTP wait must time out"
                );
            } else {
                caller.abort();
            }
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(10), done_rx)
                    .await
                    .expect("completion deadline")
                    .expect("result")
                    .expect("deployment")
                    .is_some()
            );
            service
                .try_acquire_process_release_lock("hot-cancel")
                .await
                .expect("released ownership")
                .finish()
                .await
                .expect("finish");
            server.abort();
        }
    }
    #[tokio::test]
    async fn readiness_is_required_and_operation_is_rechecked_after_ready() {
        use axum::{Json, Router, routing::get};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let ready = Arc::new(AtomicBool::new(false));
        let replace_on_ready = Arc::new(AtomicBool::new(false));
        let replaced = Arc::new(AtomicBool::new(false));
        let status_reads = Arc::new(AtomicUsize::new(0));
        let status_replaced = replaced.clone();
        let status_counter = status_reads.clone();
        let ready_value = ready.clone();
        let ready_replaces = replace_on_ready.clone();
        let ready_replaced = replaced.clone();
        let router = Router::new()
            .route("/v1/deploy/status", get(move || {
                let replaced = status_replaced.clone();
                let counter = status_counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({"data":{
                        "protocol_version":4,"phase":"running","operation":{
                            "operation_id":if replaced.load(Ordering::SeqCst) {"foreign"} else {"operation"},
                            "request_release_id":"request","deployment_generation_id":"generation",
                            "artifact_release_id":"artifact","deploy_stage":"succeeded","persisted":true,
                            "phase":"running","error":null
                        }
                    }}))
                }
            }))
            .route("/ready", get(move || {
                let ready = ready_value.clone();
                let replace = ready_replaces.clone();
                let replaced = ready_replaced.clone();
                async move {
                    if !ready.load(Ordering::SeqCst) {
                        return (axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            Json(serde_json::json!({"status":"not_ready","phase":"running"})));
                    }
                    if replace.load(Ordering::SeqCst) { replaced.store(true, Ordering::SeqCst); }
                    (axum::http::StatusCode::OK, Json(serde_json::json!({"status":"ready","phase":"running"})))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listen");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        let root = tempfile::tempdir().expect("root");
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root.path(), runtime.clone());
        let task = super::HotDeploymentTask {
            runtime,
            access_mode: crate::config::AppAccessMode::Docker,
            app_id: "ready-test".into(),
            url: "http://artifact".into(),
            release_id: "request".into(),
            sha256: String::new(),
            base: format!("http://{address}"),
            token: "token".into(),
            env_snapshot: Default::default(),
            generation_id: "generation".into(),
            operation_id: "operation".into(),
            operation: service
                .acquire_process_release_lock("ready-test")
                .await
                .expect("lease"),
        };
        let client = super::admin_client().expect("client");
        assert!(
            task.read_ready_operation(&client)
                .await
                .expect("probe")
                .is_none(),
            "Running while /ready returns 503 must not complete"
        );
        assert_eq!(status_reads.load(Ordering::SeqCst), 1);
        ready.store(true, Ordering::SeqCst);
        assert!(
            task.read_ready_operation(&client)
                .await
                .expect("probe")
                .is_some()
        );
        assert_eq!(
            status_reads.load(Ordering::SeqCst),
            3,
            "successful readiness requires the second operation read"
        );
        replace_on_ready.store(true, Ordering::SeqCst);
        assert!(
            task.read_ready_operation(&client)
                .await
                .expect("probe")
                .is_none(),
            "another operation appearing after /ready must not complete this one"
        );
        assert_eq!(status_reads.load(Ordering::SeqCst), 5);
        task.operation.finish().await.expect("finish");
        server.abort();
    }
}
