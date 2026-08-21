//! userApp 文件域转发层（rcoder 侧编排，实际处理在 per-app 开发容器内 file-server）。
//!
//! - [`forward`]：`/api/userapp/{*rest}` 通配透传 + `/api/computer/*` 拦截层
//!   （`X-Service-Type: userapp` 分流，反向代理转来的 TS 老路径原样透传）
//! - 本模块：`POST /api/userapp/workspace`（创建项目显式入口——ensure 开发容器 +
//!   容器内建 workspace 目录 + metadata 注册 owner），对齐 computer 域
//!   create-workspace 起手先例
//!
//! 容器定位/创建复用 [`crate::userapp_publish::agent_runner::ensure_userapp_builder`]
//! （幂等；注册 state.projects 防孤立清理）。

mod forward;

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::routing::Router;
use axum::routing::{any, post};
use serde::Deserialize;
use serde_json::json;
use tracing::info;

use crate::router::AppState;
use crate::userapp_publish::agent_runner::{dev_file_server_addr, ensure_userapp_builder};
use crate::{AppError, HttpResult};

// 分流 header 常量（X-Service-Type / X-App-Id）定义在 forward.rs（pub const，文档
// 与跨模块引用锚点）；本模块仅转发 computer_intercept 拦截层给主 Router 装配。
pub(crate) use forward::computer_intercept;

/// userApp 域转发路由（挂 rcoder 主 Router；`/api/userapp` 族不再来自 file-server
/// 本地路由——C1 合并时已排除）。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/userapp/workspace", post(create_workspace))
        .route("/api/userapp/{*rest}", any(forward::forward_userapp))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceBody {
    /// UserApp 应用 ID（同时是开发容器与 workspace 目录的标识）
    pub app_id: String,
    /// 用户 ID（owner，落 userapp_metadata；发布编排与 apps 代理 URL 拼接使用）
    pub user_id: String,
}

/// `POST /api/userapp/workspace`：创建项目显式入口（幂等）。
///
/// 1. ensure 该 app 的开发容器（UserAppBuilder，per-app RWO 卷；K8s 走 STS+PVC
///    ensure，Docker 走 per-app bind）
/// 2. 容器内 file-server 幂等建 workspace 目录 `{USERAPP_WORKSPACE_DIR}/{appId}`
///    （execute-command 等接口要求目录已存在）
/// 3. metadata 注册 owner user_id（name 空 = 开发期；部署 create_app 后补全）
#[utoipa::path(
    post,
    path = "/api/userapp/workspace",
    request_body = CreateWorkspaceBody,
    responses(
        (status = 200, description = "创建成功（幂等，重复调用安全）", body = HttpResult<serde_json::Value>),
        (status = 400, description = "参数校验失败", body = HttpResult<String>),
        (status = 502, description = "开发容器不可达", body = HttpResult<String>)
    ),
    tag = "UserApp",
    operation_id = "create_userapp_workspace",
    summary = "创建 UserApp 项目工作区（ensure 开发容器 + 建目录 + 注册 owner）"
)]
pub(crate) async fn create_workspace(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateWorkspaceBody>,
) -> Result<HttpResult<serde_json::Value>, AppError> {
    shared_types::validate_identifier(&body.app_id, "app_id")
        .map_err(|e| AppError::bad_request(&e))?;
    shared_types::validate_identifier(&body.user_id, "user_id")
        .map_err(|e| AppError::bad_request(&e))?;

    // 1. ensure 开发容器（幂等；注册 state.projects）
    let info = ensure_userapp_builder(&state, &body.app_id)
        .await
        .map_err(|e| {
            tracing::error!(
                "[USERAPP_FORWARD] ensure dev container failed: app_id={}: {e:#}",
                body.app_id
            );
            AppError::with_message(
                shared_types::error_codes::ERR_CONTAINER_ERROR,
                format!("ensure dev container failed: {e:#}"),
            )
        })?;

    // 2. 容器内建 workspace 目录（幂等）
    let addr = dev_file_server_addr(&state, &info);
    let ensure_resp = crate::http_client::shared_client()
        .post(format!("{addr}/api/userapp/ensure-workspace"))
        .json(&json!({"appId": body.app_id, "userId": body.user_id}))
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(
                "[USERAPP_FORWARD] ensure-workspace call failed: app_id={}: {e}",
                body.app_id
            );
            AppError::with_message(
                shared_types::error_codes::ERR_CONTAINER_ERROR,
                format!("dev container ensure-workspace failed: {e}"),
            )
        })?;
    if !ensure_resp.status().is_success() {
        let status = ensure_resp.status();
        let text = ensure_resp.text().await.unwrap_or_default();
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_CONTAINER_ERROR,
            format!("ensure-workspace returned {status}: {text}"),
        ));
    }

    // 3. metadata 注册 owner（部署前即可被发布编排/apps URL 拼接查到）
    state
        .app_service
        .record_dev_registration(&body.app_id, &body.user_id)
        .await
        .map_err(|e| {
            tracing::warn!(
                "[USERAPP_FORWARD] record dev registration failed: app_id={}: {e}",
                body.app_id
            );
            AppError::with_message(
                shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
                format!("record dev registration failed: {e}"),
            )
        })?;

    info!(
        "[USERAPP_FORWARD] workspace created: app_id={}, user_id={}, container={}, ip={}",
        body.app_id, body.user_id, info.container_name, info.container_ip
    );
    Ok(HttpResult::success(json!({
        "appId": body.app_id,
        "userId": body.user_id,
        "containerName": info.container_name,
        "containerIp": info.container_ip,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_workspace_body_is_camel_case() {
        let raw = serde_json::json!({"appId": "app-1", "userId": "u1"});
        let body: CreateWorkspaceBody = serde_json::from_value(raw).expect("deserialize");
        assert_eq!(body.app_id, "app-1");
        assert_eq!(body.user_id, "u1");
    }
}
