//! 热部署（deploy_mode=hot）：调容器内 app-cli server 的 `/v1/deploy` 端点
//! 原地换应用——不换 Pod、PG/ttyd/dbx 不断连，仅应用服务切换。
//!
//! 定位是**优化路径**：一切前置不满足（app 不存在/不在跑/容器未配
//! `APP_CLI_DEPLOY_TOKEN`/端点不可达——旧镜像无此端点）自动回退换 Pod 权威链；
//! 受理后失败（部署失败/超时）保留现场报错（旧制品 URL 重发即回滚，对齐
//! activate 失败语义）。
//!
//! 成功后把部署三元组经 `update_env_configmap` 收敛进 ConfigMap（K8s-only，
//! 不触碰 Deployment → 无 Recreate）：Pod 重建时 server 按 env 恢复最新版本，
//! 热部署的换代效果不因重建丢失。

use std::time::Duration;

use tracing::{info, warn};

use crate::error::AppOperationError;
use crate::error::AppResult;
use crate::models::AppRuntimeInfo;
use crate::service::AppService;

/// 热部署受理/轮询端口（app-cli 管理 API 常量对齐）。
const APP_CLI_ADMIN_PORT: u16 = 3010;
/// 轮询间隔/预算（对齐 wait_app_ready 语义）。
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
        let token = self
            .runtime
            .get_app_container_spec(app_id)
            .await
            .ok()
            .and_then(|spec| spec.env)
            .as_ref()
            .and_then(|env| {
                env.get("APP_CLI_DEPLOY_TOKEN")
                    .cloned()
                    .filter(|t| !t.trim().is_empty())
            });
        let Some(token) = token else {
            warn!(
                "[APP] hot deploy fallback: APP_CLI_DEPLOY_TOKEN not set on app {app_id} \
                 (server-form image required)"
            );
            return Ok(None);
        };

        // 受理
        let base = format!("http://{ip}:{APP_CLI_ADMIN_PORT}");
        let body = serde_json::json!({
            "url": url,
            "release_id": release_id,
            "sha256": if sha256.is_empty() { None } else { Some(sha256) },
        });
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
                return Err(AppOperationError::Conflict(
                    "hot deploy already in progress on container".to_string(),
                ));
            }
            Ok(r) => {
                warn!(
                    "[APP] hot deploy fallback: /v1/deploy returned {} (server-form image required)",
                    r.status()
                );
                return Ok(None);
            }
            Err(e) => {
                warn!("[APP] hot deploy fallback: connect {base} failed: {e}");
                return Ok(None);
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
            let phase = match resp {
                Ok(r) if r.status().is_success() => r
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v.get("phase").and_then(|p| p.as_str()).map(str::to_string)),
                _ => None,
            };
            match phase.as_deref() {
                Some("running") => break,
                Some("failed") => {
                    let error = "hot deploy failed on container (see app-cli logs; \
                                 redeploy old artifact URL to roll back)"
                        .to_string();
                    return Err(AppOperationError::Backend(error));
                }
                // deploying/orchestrating/idle（受理竞态窗口）→ 继续等
                _ => {}
            }
        }

        // 收敛 env 三元组进 ConfigMap（不触发 Recreate）：Pod 重建恢复最新版本
        self.converge_deploy_env_after_hot(app_id, url, release_id, sha256)
            .await;
        info!("[APP] hot deploy done (pod kept): app_id={app_id}, release_id={release_id}");
        Ok(Some(()))
    }

    /// 热部署成功后的 env 收敛：live env 读回 → 剥离历史三元组 → 注入本次
    /// 三元组 → 仅 apply ConfigMap。读回失败仅 warn（收敛是尽力而为——最坏
    /// 情况 Pod 重建回旧版本，不影响当前运行）。
    async fn converge_deploy_env_after_hot(
        &self,
        app_id: &str,
        url: &str,
        release_id: &str,
        sha256: &str,
    ) {
        let env = match self.runtime.get_app_container_spec(app_id).await {
            Ok(spec) => spec.env.unwrap_or_default(),
            Err(e) => {
                warn!(
                    "[APP] hot deploy env convergence skipped (live read failed): app_id={app_id}: {e}"
                );
                return;
            }
        };
        let mut env: std::collections::HashMap<String, String> = env.into_iter().collect();
        crate::release_flow::identity::strip_release_identity(&mut env);
        env.insert("APP_DEPLOY_URL".to_string(), url.to_string());
        env.insert("APP_RELEASE_ID".to_string(), release_id.to_string());
        env.insert("APP_DEPLOY_SHA256".to_string(), sha256.to_string());
        if let Err(e) = self.runtime.update_env_configmap(app_id, &env).await {
            warn!(
                "[APP] hot deploy env convergence failed (pod rebuild restores old version): \
                 app_id={app_id}: {e}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::models::StartAppRequest;
    use crate::test_support::{MockRuntime, test_service};
    use std::sync::Arc;

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

    /// deploy_mode wire：默认缺省（pod）+ hot 受理。
    #[test]
    fn deploy_mode_wire_default_and_hot() {
        let req: StartAppRequest =
            serde_json::from_str(r#"{"url":"http://x/p.zip"}"#).expect("parse");
        assert!(req.deploy_mode.is_none(), "default must be absent (= pod)");

        let req: StartAppRequest =
            serde_json::from_str(r#"{"url":"http://x/p.zip","deploy_mode":"hot"}"#).expect("parse");
        assert_eq!(req.deploy_mode, Some(crate::models::DeployMode::Hot));

        // 非法值拒绝（枚举校验）
        assert!(
            serde_json::from_str::<StartAppRequest>(
                r#"{"url":"http://x/p.zip","deploy_mode":"fast"}"#
            )
            .is_err()
        );
    }
}
