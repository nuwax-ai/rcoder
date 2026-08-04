//! 驱动 app_manager 的发布生命周期:ensure_app(幂等建/取运行单元)、wait_app_ready(轮询就绪)。
//!
//! 编排(何时调、终态/取消/回滚收敛)在 `super::orchestrator`;本模块只封装与 app_manager 的交互细节。

use std::time::Duration;

use anyhow::{Result, anyhow};

use app_manager::models::commons::{AppStatus, ExposeType, HealthCheckType};
use app_manager::models::{CreateAppRequest, HealthCheckConfig, PortConfig};

use crate::router::AppState;

use super::task::PublishTask;

/// app-runtime 容器公网端口(pingap 监听,对外 Service + PortConfig 用)。
const APP_HTTP_PORT: u16 = 9080;
/// app-cli 管理 API 端口(K8s 探针打这里:app-cli 自身提供 /health+/ready,不强依赖后端 app)。
const APP_CLI_ADMIN_PORT: u16 = 3010;
/// app-cli 提供的探针路径(liveness=进程活,readiness=初始化完成/可选桥接后端)。
const APP_LIVENESS_PATH: &str = "/health";
const APP_READINESS_PATH: &str = "/ready";
/// 就绪轮询间隔。
const READY_POLL_INTERVAL_SECS: u64 = 3;
/// 就绪轮询总超时(activate 后 app 启动 + 健康检查窗口)。
const APP_READY_TIMEOUT_SECS: u64 = 600;

/// 确保 app 计算单元存在:不存在则 create_app(幂等;image/ports 首次设定后恒定)。
pub(super) async fn ensure_app(state: &AppState, rcoder_app_id: &str, name: &str, image: &str) -> Result<()> {
    match state.app_service.get_app(rcoder_app_id).await {
        Ok(_) => {
            // app 已存在:image/ports/probes 首次设定后恒定,不自动 reconcile(#14)。
            // 注:app_service trait 只暴露运行时信息(AppRuntimeInfo,无 image 字段),无法在此
            // 直接比对存储镜像;改为记录期望 image,平台升级 app-runtime 后运维可据日志发现滞后。
            tracing::info!(
                app_id = %rcoder_app_id,
                desired_image = %image,
                "[USERAPP_PUBLISH] app already exists; image/ports/probes are constant after first \
                 create and will NOT be reconciled to the desired image"
            );
            return Ok(()); // 已存在
        }
        Err(e) if is_not_found(&e) => {} // 不存在 → create
        Err(e) => return Err(anyhow!("get_app: {e}")),
    }
    let req = CreateAppRequest {
        app_id: Some(rcoder_app_id.to_string()),
        name: name.to_string(),
        image: image.to_string(),
        command: None,
        env: None,
        secrets: None,
        resources: None,
        ports: Some(vec![PortConfig {
            name: "http".to_string(),
            port: APP_HTTP_PORT,
            expose_type: ExposeType::Http,
            strip_prefix: None,
        }]),
        // 探针打 app-cli 的 3010 管理 API(非 pingap 9080):app-cli 自身提供 /health(liveness,
        // 进程活,后端有 bug 也不杀容器)+ /ready(readiness,默认 app-cli 就绪/可选桥接后端)。
        // 不再硬编码 /api/rust/ready(旧 bug:与实际后端语言无关,且强依赖后端实现该路径)。
        health_check: Some(HealthCheckConfig {
            check_type: HealthCheckType::Http,
            path: Some(APP_READINESS_PATH.to_string()),
            liveness_path: Some(APP_LIVENESS_PATH.to_string()),
            port: Some(APP_CLI_ADMIN_PORT),
        }),
        tenant_id: None,
        space_id: None,
        // 发布编排创建的 UserApp 默认参与闲置回收（= 免费用户语义）；如需付费常驻由调用方另行 update。
        recycle_enabled: None,
        idle_timeout_seconds: None,
    };
    state
        .app_service
        .create_app(req)
        .await
        .map_err(|e| anyhow!("create_app: {e}"))?;
    Ok(())
}

/// 轮询 app 到 status=Running 且 health 非 Unhealthy;超时或进入 Error 则失败。
pub(super) async fn wait_app_ready(state: &AppState, rcoder_app_id: &str, task: &PublishTask) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(APP_READY_TIMEOUT_SECS);
    loop {
        if task.is_cancelled() {
            return Err(anyhow!("publish cancelled by user"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!(
                "app readiness poll timed out after {APP_READY_TIMEOUT_SECS}s"
            ));
        }
        let info = state
            .app_service
            .get_app(rcoder_app_id)
            .await
            .map_err(|e| anyhow!("get_app poll: {e}"))?;
        if info.status == AppStatus::Error {
            return Err(anyhow!(
                "app entered Error state (health={})",
                info.health.status
            ));
        }
        if info.status == AppStatus::Running && info.health.status != "Unhealthy" {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(READY_POLL_INTERVAL_SECS)).await;
    }
}

/// app_manager 错误是否 "app 不存在"(get_app 判存性用)。
fn is_not_found(e: &app_manager::error::AppOperationError) -> bool {
    matches!(e, app_manager::error::AppOperationError::NotFound(_))
}

/// file-server project_id → rcoder app_id(强制 `app-` 前缀,已带则原样)。
pub(super) fn rcoder_app_id(app_id: &str) -> String {
    if app_id.starts_with("app-") {
        app_id.to_string()
    } else {
        format!("app-{app_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::rcoder_app_id;

    #[test]
    fn rcoder_app_id_prepends_prefix_when_missing() {
        assert_eq!(rcoder_app_id("userapp-e2e"), "app-userapp-e2e");
    }

    #[test]
    fn rcoder_app_id_is_idempotent_when_prefixed() {
        assert_eq!(rcoder_app_id("app-userapp-e2e"), "app-userapp-e2e");
    }
}
