//! Cold deployment waits for the matching operation's durable artifact-stage result.
//! Request release IDs and manifest identities are diagnostic values, never fences.
//! Old pods may answer during replacement; their terminal states cannot complete or
//! fail a newer operation. Business readiness remains a separate runtime concern.

use std::time::Duration;

use tracing::{info, warn};

use crate::error::AppOperationError;
use crate::error::AppResult;
use crate::models::AppRuntimeInfo;
use crate::service::AppService;

/// Reconciliation has a hard ceiling; HTTP response waiting remains 300 seconds.
// HTTP waiting is bounded separately; the owned coordinator retains its lease.
const DEPLOY_STAGE_BUDGET: Duration = Duration::from_secs(30 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// app-cli 管理 API 端口（容器内恒绑 0.0.0.0）。
const APP_CLI_ADMIN_PORT: u16 = shared_types::APP_CLI_ADMIN_PORT;

/// `/v1/deploy/status` 响应的有效载荷（信封裁剪后供判定核心消费）。
#[derive(Debug, Default, Clone)]
pub(crate) struct DeployStatusProbe {
    pub protocol_version: Option<u32>,
    pub operation: Option<shared_types::AppDeploymentOperation>,
}

/// Parse either envelope or bare JSON without weakening the operation contract.
pub(crate) fn parse_deploy_status(value: &serde_json::Value) -> DeployStatusProbe {
    let data = value.get("data").unwrap_or(value);
    DeployStatusProbe {
        protocol_version: data
            .get("protocol_version")
            .and_then(|v| v.as_u64())
            .and_then(|v| u32::try_from(v).ok()),
        operation: data
            .get("operation")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
    }
}

/// 等待核心判定（纯函数，判据矩阵单测入口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StageVerdict {
    /// 部署段完成（编排已启动）——同步等待成功返回。
    Done,
    /// 部署段未完成/不可观测——继续轮询。
    Pending,
    /// 容器侧部署失败，携带 error 文本。
    Failed(String),
}

/// Artifact-stage completion is immutable even if later business orchestration fails.
pub(crate) fn judge_stage(probe: &DeployStatusProbe, expected_operation_id: &str) -> StageVerdict {
    use shared_types::AppDeploymentStage;
    let Some(operation) = &probe.operation else {
        return StageVerdict::Pending;
    };
    if operation.operation_id != expected_operation_id {
        return StageVerdict::Pending;
    }
    if probe.protocol_version != Some(shared_types::APP_CLI_UNIFIED_DEPLOY_PROTOCOL)
        || operation.deployment_generation_id != expected_operation_id
    {
        return StageVerdict::Failed("invalid cold deployment operation contract".into());
    }
    if !operation.persisted {
        return StageVerdict::Pending;
    }
    match operation.deploy_stage {
        AppDeploymentStage::Succeeded => StageVerdict::Done,
        AppDeploymentStage::Pending => StageVerdict::Pending,
        AppDeploymentStage::Failed => StageVerdict::Failed(
            operation
                .error
                .clone()
                .unwrap_or_else(|| "artifact deployment failed".into()),
        ),
    }
}

impl AppService {
    /// 冷部署同步等待：部署段完成（编排启动）即 Ok。超时/容器 Error 态/部署
    /// 期被删/容器侧 Failed 即 Err——错误信息面向 Java/前端排查直读。
    pub(crate) async fn wait_deploy_stage(
        &self,
        app_id: &str,
        operation_id: &str,
        operation_guard: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        let mut client: Option<reqwest::Client> = None;
        let deadline = tokio::time::Instant::now() + DEPLOY_STAGE_BUDGET;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(AppOperationError::Backend(format!(
                    "deploy stage not confirmed within {}s (app {app_id}, operation {operation_id}); \
                     container may still be deploying in background — GET /apps/{app_id} to \
                     check status (or app logs) before retrying",
                    DEPLOY_STAGE_BUDGET.as_secs()
                )));
            }

            let desired = match self.runtime.app_env_snapshot(app_id).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    warn!(app_id, operation_id, %error, "Deployment configuration unavailable; retaining reconciliation ownership");
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
            };
            let token = desired
                .env
                .get("APP_CLI_DEPLOY_TOKEN")
                .filter(|t| !t.trim().is_empty());
            if desired
                .env
                .get(shared_types::APP_DEPLOY_OPERATION_ID)
                .map(String::as_str)
                != Some(operation_id)
            {
                return Err(AppOperationError::Conflict(format!(
                    "deployment operation {operation_id} was superseded on app {app_id}"
                )));
            }
            let app: AppRuntimeInfo = match self.get_app(app_id).await {
                Ok(app) => app,
                Err(AppOperationError::NotFound(_)) => {
                    return Err(AppOperationError::Backend(format!(
                        "app {app_id} was deleted while waiting for deploy stage"
                    )));
                }
                Err(e) => {
                    warn!(
                        app_id = app_id,
                        %e,
                        "deploy stage poll transient error, retrying within budget"
                    );
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
            };

            let ip = app
                .health
                .instance
                .as_ref()
                .map(|instance| instance.ip.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let Some(ip) = ip {
                let client = match client {
                    Some(ref client) => client,
                    ref mut slot @ None => {
                        slot.insert(reqwest::Client::builder().no_proxy().build().map_err(
                            |error| {
                                AppOperationError::Backend(format!(
                                    "build direct container deployment status client: {error}"
                                ))
                            },
                        )?)
                    }
                };
                let mut req = client
                    .get(format!("http://{ip}:{APP_CLI_ADMIN_PORT}/v1/deploy/status"))
                    .timeout(STATUS_REQUEST_TIMEOUT);
                if let Some(token) = token {
                    req = req.header("X-Deploy-Token", token);
                }
                if let Ok(resp) = req.send().await
                    && resp.status().is_success()
                    && let Ok(body) = resp.json::<serde_json::Value>().await
                {
                    match judge_stage(&parse_deploy_status(&body), operation_id) {
                        StageVerdict::Done => {
                            info!(
                                "[APP] deploy stage done (orchestration started): \
                                 app_id={app_id}, operation_id={operation_id}"
                            );
                            return Ok(());
                        }
                        StageVerdict::Failed(err) => {
                            // Only a matching durable terminal frees the operation for manual retry.
                            if parse_deploy_status(&body).operation.is_some_and(|op| {
                                op.persisted
                                    && op.deploy_stage == shared_types::AppDeploymentStage::Failed
                            }) {
                                operation_guard.mark_completed();
                            }
                            return Err(AppOperationError::Backend(format!(
                                "deploy stage failed on app {app_id} (operation {operation_id}): {err}"
                            )));
                        }
                        StageVerdict::Pending => {}
                    }
                }
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::{AppCliDeployPhase as Phase, AppDeploymentStage as Stage};

    fn operation_probe(op: &str, stage: Stage, phase: Phase) -> DeployStatusProbe {
        parse_deploy_status(&serde_json::json!({"data": {
            "protocol_version": shared_types::APP_CLI_UNIFIED_DEPLOY_PROTOCOL,
            "phase": phase,
            "release_id": "manifest-build",
            "request_release_id": "request-build",
            "operation": {
                "operation_id": op, "deployment_generation_id": op,
                "request_release_id": "request-build", "artifact_release_id": "manifest-build",
                "deploy_stage": stage, "persisted": true, "phase": phase, "error": "deployment failed"
            }
        }}))
    }

    #[test]
    fn cold_stage_uses_operation_not_request_or_manifest_identity() {
        assert_eq!(
            judge_stage(
                &operation_probe("op-new", Stage::Succeeded, Phase::Orchestrating),
                "op-new"
            ),
            StageVerdict::Done
        );
        assert_eq!(
            judge_stage(
                &operation_probe("op-old", Stage::Succeeded, Phase::Running),
                "op-new"
            ),
            StageVerdict::Pending
        );
        assert_eq!(
            judge_stage(
                &operation_probe("op-old", Stage::Failed, Phase::Failed),
                "op-new"
            ),
            StageVerdict::Pending
        );
    }

    #[test]
    fn healthy_old_service_cannot_hide_failed_deployment() {
        assert!(matches!(
            judge_stage(
                &operation_probe("op-new", Stage::Failed, Phase::Running),
                "op-new"
            ),
            StageVerdict::Failed(_)
        ));
        assert_eq!(
            judge_stage(
                &operation_probe("op-new", Stage::Pending, Phase::Running),
                "op-new"
            ),
            StageVerdict::Pending
        );
        // Cold acceptance guarantees artifact activation, not later business readiness.
        assert_eq!(
            judge_stage(
                &operation_probe("op-new", Stage::Succeeded, Phase::Failed),
                "op-new"
            ),
            StageVerdict::Done
        );
    }

    #[test]
    fn missing_operation_never_falls_back_to_release_id() {
        let probe = parse_deploy_status(
            &serde_json::json!({"phase":"running", "release_id":"op-new", "request_release_id":"op-new"}),
        );
        assert!(!matches!(judge_stage(&probe, "op-new"), StageVerdict::Done));
    }

    #[test]
    fn both_wire_shapes_preserve_operation_contract() {
        let probe = operation_probe("op", Stage::Pending, Phase::Deploying);
        let op = probe.operation.unwrap();
        let bare =
            serde_json::json!({"protocol_version": 4, "operation": op, "phase": "deploying"});
        assert_eq!(
            parse_deploy_status(&bare).operation.unwrap().operation_id,
            "op"
        );
        assert!(
            parse_deploy_status(&serde_json::json!({"data":null}))
                .operation
                .is_none()
        );
    }
}
