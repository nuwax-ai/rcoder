//! 部署段等待器（冷部署同步等待点）：轮询容器内 app-cli `GET /v1/deploy/status`
//! 直到状态机离开 Deploying——部署段（下载/sha256 校验/解压/换 code）成功且
//! 编排已启动（Orchestrating/Running）即返回成功；`Failed` 立即失败并透传容器
//! 侧 error 文本。
//!
//! 等待边界：**不等用户服务启动/bridge 探活**——服务起不起是用户代码域
//! （readiness 探针照常摘流/恢复流量，kubelet 自愈不受影响），rcoder 只担保
//! 平台责任段。成功路径 20~60s（容器调度 + 下载解压），预算 300s 仅界定失败
//! 发现上限。返回成功 ≠ 立即接流量（readiness 摘流窗口内访问为 503/502）。
//!
//! 兼容性：旧 app-runtime 镜像首次部署下载期 3010 由 LivenessHold 占位（其余
//! 路径恒 503）、部署失败 supervisord 循环重启（端点间歇拒连）——探测不可达
//! /未知相位一律按「继续等」处理：成功路径不受影响，失败上报降级为超时。

use std::time::Duration;

use tracing::{info, warn};

use crate::error::AppOperationError;
use crate::error::AppResult;
use crate::models::AppRuntimeInfo;
use crate::models::AppStatus;
use crate::service::AppService;

/// 等待预算/节奏（沿用原 wait_app_ready 300s 量级：只影响失败发现上限，
/// 成功路径由部署段实际时长决定）。
const DEPLOY_STAGE_BUDGET: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// app-cli 管理 API 端口（容器内恒绑 0.0.0.0）。
const APP_CLI_ADMIN_PORT: u16 = shared_types::APP_CLI_ADMIN_PORT;

/// `/v1/deploy/status` 响应的有效载荷（信封裁剪后供判定核心消费）。
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct DeployStatusProbe {
    /// 状态机相位；`None` = 端点不可达/响应无相位/未知相位字符串（旧镜像）。
    pub phase: Option<shared_types::AppCliDeployPhase>,
    /// `Failed` 相位的原因文本（`DeployStatus.error` 独立字段）。
    pub error: Option<String>,
    /// 当前代 release_id（`Deploying` 期间滞后为上一代值，`Orchestrating` 起为本代）。
    pub release_id: Option<String>,
}

/// 解析 status 响应：信封形态 `data.*` 与旧裸顶层形态双兼容（app-cli 与
/// rcoder 任一侧先发版都不破坏轮询；与 hot_deploy 链共用）。
pub(crate) fn parse_deploy_status(value: &serde_json::Value) -> DeployStatusProbe {
    let data = value.get("data").unwrap_or(value);
    DeployStatusProbe {
        phase: data
            .get("phase")
            .and_then(|p| p.as_str())
            .and_then(|s| s.parse::<shared_types::AppCliDeployPhase>().ok()),
        error: data
            .get("error")
            .and_then(|e| e.as_str())
            .map(str::to_string),
        release_id: data
            .get("release_id")
            .and_then(|r| r.as_str())
            .map(str::to_string),
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

/// 判据（穷尽 match：新增 `AppCliDeployPhase` 变体时编译错，强制显式决策
/// 该相位算部署段完成还是继续等）：
/// - `Orchestrating | Running` 且 release_id 已切到本代 → Done（release_id
///   交叉校验防上一代残留相位误判；`Deploying` 期间该字段滞后旧值）；
/// - `Idle | Deploying` → Pending；
/// - `Failed` → Failed（透传 error）；
/// - `None`（不可达/未知相位——旧镜像）→ Pending。
pub(crate) fn judge_stage(probe: &DeployStatusProbe, expect_release_id: &str) -> StageVerdict {
    use shared_types::AppCliDeployPhase as Phase;
    match probe.phase {
        Some(Phase::Orchestrating | Phase::Running) => {
            if probe.release_id.as_deref() == Some(expect_release_id) {
                StageVerdict::Done
            } else {
                StageVerdict::Pending
            }
        }
        Some(Phase::Idle | Phase::Deploying) => StageVerdict::Pending,
        Some(Phase::Failed) => StageVerdict::Failed(
            probe
                .error
                .clone()
                .unwrap_or_else(|| "unknown deploy failure".to_string()),
        ),
        None => StageVerdict::Pending,
    }
}

impl AppService {
    /// 冷部署同步等待：部署段完成（编排启动）即 Ok。超时/容器 Error 态/部署
    /// 期被删/容器侧 Failed 即 Err——错误信息面向 Java/前端排查直读。
    pub(crate) async fn wait_deploy_stage(&self, app_id: &str, release_id: &str) -> AppResult<()> {
        // X-Deploy-Token：GET status 本无鉴权，带上对齐 hot_deploy 链；读不到
        // （旧镜像未配）裸查。循环外读一次。
        let token = self
            .runtime
            .get_app_container_spec(app_id)
            .await
            .ok()
            .and_then(|spec| spec.env)
            .as_ref()
            .and_then(|env| env.get("APP_CLI_DEPLOY_TOKEN").cloned())
            .filter(|t| !t.trim().is_empty());

        let mut client: Option<reqwest::Client> = None;
        let deadline = tokio::time::Instant::now() + DEPLOY_STAGE_BUDGET;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(AppOperationError::Backend(format!(
                    "deploy stage not confirmed within {}s (app {app_id}, release {release_id}); \
                     container may still be deploying in background — GET /apps/{app_id} to \
                     check status (or app logs) before retrying",
                    DEPLOY_STAGE_BUDGET.as_secs()
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
            if app.status == AppStatus::Error {
                return Err(AppOperationError::Backend(format!(
                    "app {app_id} entered Error state while waiting for deploy stage \
                     (health={})",
                    app.health.status
                )));
            }

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
                if let Some(token) = token.as_deref() {
                    req = req.header("X-Deploy-Token", token);
                }
                if let Ok(resp) = req.send().await
                    && resp.status().is_success()
                    && let Ok(body) = resp.json::<serde_json::Value>().await
                {
                    match judge_stage(&parse_deploy_status(&body), release_id) {
                        StageVerdict::Done => {
                            info!(
                                "[APP] deploy stage done (orchestration started): \
                                 app_id={app_id}, release_id={release_id}"
                            );
                            return Ok(());
                        }
                        StageVerdict::Failed(err) => {
                            return Err(AppOperationError::Backend(format!(
                                "deploy stage failed on app {app_id} (release {release_id}): {err}"
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
    use crate::test_support::{MockRuntime, test_service};
    use shared_types::AppCliDeployPhase as Phase;
    use std::sync::Arc;

    fn probe(
        phase: Option<Phase>,
        release_id: Option<&str>,
        error: Option<&str>,
    ) -> DeployStatusProbe {
        DeployStatusProbe {
            phase,
            release_id: release_id.map(str::to_string),
            error: error.map(str::to_string),
        }
    }

    /// 判据矩阵：部署段完成的唯一口径 = 编排相位 + release_id 已切本代。
    #[test]
    fn judge_stage_matrix() {
        // 成功：Orchestrating/Running + release_id 匹配
        assert_eq!(
            judge_stage(
                &probe(Some(Phase::Orchestrating), Some("rel-2"), None),
                "rel-2"
            ),
            StageVerdict::Done
        );
        assert_eq!(
            judge_stage(&probe(Some(Phase::Running), Some("rel-2"), None), "rel-2"),
            StageVerdict::Done
        );
        // 编排相位但 release_id 还是上一代（Deploying 滞后窗口）→ 继续等
        assert_eq!(
            judge_stage(
                &probe(Some(Phase::Orchestrating), Some("rel-1"), None),
                "rel-2"
            ),
            StageVerdict::Pending
        );
        assert_eq!(
            judge_stage(&probe(Some(Phase::Running), None, None), "rel-2"),
            StageVerdict::Pending
        );
        // 部署段进行中/未开始 → 继续等
        assert_eq!(
            judge_stage(&probe(Some(Phase::Deploying), Some("rel-2"), None), "rel-2"),
            StageVerdict::Pending
        );
        assert_eq!(
            judge_stage(&probe(Some(Phase::Idle), None, None), "rel-2"),
            StageVerdict::Pending
        );
        // 失败：透传容器 error；缺 error 文本兜底
        assert_eq!(
            judge_stage(
                &probe(Some(Phase::Failed), Some("rel-2"), Some("download 404")),
                "rel-2"
            ),
            StageVerdict::Failed("download 404".to_string())
        );
        assert_eq!(
            judge_stage(&probe(Some(Phase::Failed), None, None), "rel-2"),
            StageVerdict::Failed("unknown deploy failure".to_string())
        );
        // 不可观测（旧镜像下载期 503/拒连 → phase 解析为 None）→ 继续等
        assert_eq!(
            judge_stage(&probe(None, None, None), "rel-2"),
            StageVerdict::Pending
        );
    }

    /// status 解析：信封/裸顶层双兼容 + 未知相位字符串容错（继续等）。
    #[test]
    fn parse_deploy_status_shapes() {
        let envelope = serde_json::json!({
            "code": "0000", "message": "success", "tid": null, "success": true,
            "data": {
                "phase": "failed",
                "release_id": "rel-9",
                "error": "artifact sha256 mismatch: expected aa, got bb"
            }
        });
        let probe = parse_deploy_status(&envelope);
        assert_eq!(probe.phase, Some(Phase::Failed));
        assert_eq!(probe.release_id.as_deref(), Some("rel-9"));
        assert_eq!(
            probe.error.as_deref(),
            Some("artifact sha256 mismatch: expected aa, got bb")
        );

        let legacy = serde_json::json!({ "phase": "orchestrating", "release_id": "rel-1" });
        assert_eq!(
            parse_deploy_status(&legacy).phase,
            Some(Phase::Orchestrating)
        );

        // 未知相位（未来新增/大小写异常）→ None = 继续等，不炸轮询
        assert_eq!(
            parse_deploy_status(&serde_json::json!({ "data": { "phase": "migrating" } })).phase,
            None
        );
        assert_eq!(
            parse_deploy_status(&serde_json::json!({ "code": "0000", "data": null })).phase,
            None
        );
    }

    /// 容器 Error 态快速失败（不等满预算）——Mock 无 HTTP 能力，正好覆盖
    /// 循环里不依赖 3010 的失败分支。
    #[tokio::test]
    async fn wait_deploy_stage_fails_fast_on_error_state() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        runtime.deployments.insert(
            "app-broken".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "app-broken".into(),
                replicas: 1,
                ready_replicas: 0,
                phase: "Error".into(),
                ..Default::default()
            },
        );
        let svc = test_service(tmp.path(), runtime);

        let err = svc
            .wait_deploy_stage("app-broken", "rel-1")
            .await
            .expect_err("Error state must fail fast");
        assert!(
            err.to_string().contains("Error state"),
            "error should mention Error state, got: {err}"
        );
    }
}
