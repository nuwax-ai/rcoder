//! 热部署（deploy_mode=hot）：调容器内 app-cli server 的 `/v1/deploy` 端点
//! 原地换应用——不换 Pod、PG/ttyd/dbx 不断连，仅应用服务切换。
//!
//! 定位是**优化路径**：一切前置不满足（app 不存在/不在跑/容器未配
//! `APP_CLI_DEPLOY_TOKEN`/端点不可达——旧镜像无此端点）自动回退换 Pod 权威链；
//! 受理后失败（部署失败/超时）保留现场报错（旧制品 URL 重发即回滚，对齐
//! activate 失败语义）。
//!
//! 等待边界与冷部署链（[`crate::lifecycle`] wait_deploy_stage，部署段完成即
//! 返回）不同：热部署**等 `Running`**——热切换的价值就是旧服务不断流，须等
//! 新版编排完成 + bridge readiness 过了才算成功（旧服务期间一直在线）。
//!
//! 成功后把部署三元组经 `update_env_configmap` 收敛进 ConfigMap（K8s-only，
//! 不触碰 Deployment → 无 Recreate）：Pod 重建时 server 按 env 恢复最新版本，
//! 热部署的换代效果不因重建丢失。

use std::time::Duration;

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

impl AppService {
    /// 尝试热部署。`Ok(Some(()))` = 完成（调用方跳过换 Pod 链，直接进 SQL 执行
    /// 等后续）；`Ok(None)` = 前置不满足，回退换 Pod；`Err` = 受理后失败（现场
    /// 保留）。409（已有部署在进行）如实上抛冲突。
    pub(crate) async fn try_deploy_via_container_api(
        &self,
        app_id: &str,
        url: &str,
        release_id: &str,
        sha256: &str,
    ) -> AppResult<Option<()>> {
        let operation = self.try_acquire_process_release_lock(app_id).await?;
        let result = self
            .try_deploy_via_container_api_locked(app_id, url, release_id, sha256, &operation)
            .await;
        finish_hot_operation(operation, result).await
    }

    async fn try_deploy_via_container_api_locked(
        &self,
        app_id: &str,
        url: &str,
        release_id: &str,
        sha256: &str,
        operation: &crate::service::AppOperationGuard,
    ) -> AppResult<Option<()>> {
        // 前置：app 存在且 Running 且有可路由 IP
        let app: AppRuntimeInfo = match self.get_app(app_id).await {
            Ok(app) => app,
            Err(AppOperationError::NotFound(_)) => {
                info!(
                    "[APP] hot deploy fallback: app {app_id} not found (first deploy → pod path)"
                );
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let Some(ip) = app
            .health
            .instance
            .as_ref()
            .map(|instance| instance.ip.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            warn!("[APP] hot deploy fallback: app {app_id} has no ready runtime IP");
            return Ok(None);
        };

        // 前置：容器配置了部署令牌（未配置 = 端点禁用 = 镜像未升级到 server 形态）
        let env_snapshot = self
            .runtime
            .app_env_snapshot(app_id)
            .await
            .map_err(|e| AppOperationError::Backend(format!("read hot deployment env: {e}")))?;
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
            return Ok(None);
        };

        // 受理
        let base = format!("http://{ip}:{APP_CLI_ADMIN_PORT}");
        let client = reqwest::Client::new();
        let capability = client
            .get(format!("{base}/v1/deploy/status"))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| {
                AppOperationError::Backend(format!("hot deployment capability probe failed: {e}"))
            })?;
        if capability.status().as_u16() == 404 {
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
            return Ok(None);
        }
        let operation_id = uuid::Uuid::new_v4().simple().to_string();
        let body = serde_json::json!({
            "operation_id": operation_id,
            "url": url,
            "release_id": release_id,
            "sha256": if sha256.is_empty() { None } else { Some(sha256) },
        });
        operation.mark_mutating()?;
        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/deploy"))
            .timeout(Duration::from_secs(30))
            .header("X-Deploy-Token", &token)
            .json(&body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().as_u16() == 202 => {}
            Ok(r) if r.status().as_u16() == 409 => {
                operation.mark_rejected_before_mutation();
                // 透传容器侧错误体（含 "deploy in progress (phase=...)" 真实相位）
                let detail = r.text().await.unwrap_or_default();
                return Err(AppOperationError::Conflict(format!(
                    "hot deploy rejected by container: {detail}"
                )));
            }
            Ok(r) => {
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

        // 轮询到终态（running=成功——server 在编排+bridge readiness 完成后才置 Running）
        let client = reqwest::Client::new();
        let deadline = tokio::time::Instant::now() + HOT_DEPLOY_BUDGET;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(AppOperationError::Backend(
                    "hot deploy timeout (container keeps deploying in background; \
                     retry later or use pod mode)"
                        .to_string(),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            let resp = client
                .get(format!("{base}/v1/deploy/status"))
                .timeout(Duration::from_secs(10))
                .header("X-Deploy-Token", &token)
                .send()
                .await;
            let probe = match resp {
                Ok(r) if r.status().is_success() => {
                    r.json::<serde_json::Value>().await.ok().and_then(|v| {
                        observe_hot_operation(&v, &operation_id, release_id, operation)
                    })
                }
                _ => None,
            };
            // 穷尽 match：新增 AppCliDeployPhase 变体时编译错，强制同步本判据
            match probe
                .as_ref()
                .filter(|p| p.operation_id == operation_id && p.request_release_id == release_id)
                .map(|p| p.phase)
            {
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
        self.converge_deploy_env_after_hot(
            app_id,
            url,
            release_id,
            sha256,
            &env_snapshot,
            operation,
        )
        .await?;
        info!("[APP] hot deploy done (pod kept): app_id={app_id}, release_id={release_id}");
        Ok(Some(()))
    }

    /// 热部署成功后的 env 收敛：live env 读回 → 剥离历史三元组 → 注入本次
    /// 三元组 → 条件写入 ConfigMap。任何冲突或失败均显式报告，禁止伪装完全成功。
    async fn converge_deploy_env_after_hot(
        &self,
        app_id: &str,
        url: &str,
        release_id: &str,
        sha256: &str,
        snapshot: &shared_types::AppEnvSnapshot,
        operation: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        let mut env = snapshot.env.clone();
        crate::release_flow::identity::strip_release_identity(&mut env);
        env.insert("APP_DEPLOY_URL".to_string(), url.to_string());
        env.insert("APP_RELEASE_ID".to_string(), release_id.to_string());
        env.insert("APP_DEPLOY_SHA256".to_string(), sha256.to_string());
        self.runtime
            .update_env_configmap_if_version(app_id, &env, snapshot)
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
}

fn supports_hot_protocol(document: &serde_json::Value) -> bool {
    document
        .pointer("/data/protocol_version")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|version| {
            version == u64::from(shared_types::app_cli_deploy::APP_CLI_OPERATION_ID_DEPLOY_PROTOCOL)
                || version
                    == u64::from(shared_types::app_cli_deploy::APP_CLI_QUIESCENT_DEPLOY_PROTOCOL)
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

/// Only a matched, quiescent failure completes ownership. A failed operation
/// can coexist with active restoration, so its phase alone is insufficient.
fn observe_hot_operation(
    document: &serde_json::Value,
    operation_id: &str,
    release_id: &str,
    guard: &crate::service::AppOperationGuard,
) -> Option<shared_types::AppDeploymentOperation> {
    let operation: shared_types::AppDeploymentOperation =
        serde_json::from_value(document.pointer("/data/operation")?.clone()).ok()?;
    if operation.operation_id != operation_id || operation.request_release_id != release_id {
        return None;
    }
    if operation.phase == AppCliDeployPhase::Failed {
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
        for (version, supported) in [(0, false), (1, false), (2, true), (3, true), (4, false)] {
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
            .try_deploy_via_container_api("app-nope", "http://x/p.zip", "rel-1", "")
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
            .try_deploy_via_container_api("app-live", "http://x/p.zip", "rel-1", "")
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
            for (protocol, operation_id, phase, recovery, release) in [
                (3, "current", "failed", None, true),
                (3, "current", "running", Some("restored"), true),
                (3, "current", "failed", Some("failed"), true),
                (3, "current", "orchestrating", Some("pending"), false),
                (3, "current", "failed", Some("unknown"), false),
                (3, "old", "failed", None, false),
                (2, "current", "failed", None, false),
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
                    "phase":"failed","error":"download 404", "recovery": recovery.map(|status| serde_json::json!({
                        "status":status,"error":null,"database_migrations_reversed":false
                    }))
                }}});
                let observed =
                    super::observe_hot_operation(&document, "current", "release-b", &guard);
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
                    "k8s={kubernetes}, phase={phase}, recovery={recovery:?}"
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
            let result = service
                .converge_deploy_env_after_hot(
                    "hot-env",
                    "http://artifact",
                    "release-b",
                    "",
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
}
