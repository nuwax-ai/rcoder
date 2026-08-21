//! userApp 文件域转发层（rcoder 侧编排，实际处理在 per-app 开发容器内 file-server）。
//!
//! - [`forward`]：`/api/userapp/{*rest}` 通配透传 + `/api/computer/*` 拦截层
//!   （`X-Service-Type: userapp` 分流，反向代理转来的 TS 老路径原样透传）
//! - [`workspace`]：`POST /api/userapp/workspace` 创建项目显式入口
//! - [`db`]：`POST /api/userapp/db/{env}/align-credentials` PG 凭据对齐
//! - 本模块：路由聚合 + 开发容器 ensure-workspace 公共调用
//!
//! 容器定位/创建复用 [`crate::userapp_publish::agent_runner::ensure_userapp_builder`]
//! （幂等；注册 state.projects 防孤立清理）。

pub(crate) mod db;
mod forward;
pub(crate) mod workspace;

use std::sync::Arc;

use axum::routing::Router;
use axum::routing::{any, post};

use crate::router::AppState;

// 分流 header 常量（X-Service-Type / X-App-Id）定义在 forward.rs；本模块转发
// computer_intercept 拦截层与标记值常量（chat 的 body service_type 与 header 同词表）。
pub use forward::SERVICE_TYPE_USERAPP;
pub(crate) use forward::computer_intercept;

/// userApp 域转发路由（挂 rcoder 主 Router；`/api/userapp` 族不再来自 file-server
/// 本地路由——路由合并时已排除）。
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/userapp/workspace", post(workspace::create_workspace))
        .route(
            "/api/userapp/db/{env}/align-credentials",
            post(db::align_credentials),
        )
        .route("/api/userapp/{*rest}", any(forward::forward_userapp))
}

/// 容器内 file-server 幂等建 workspace 目录（execute-command 等接口的 cwd 前置；
/// create-workspace / chat 开发对话 / db 对齐共用的公共调用）。
///
/// 错误返回面向日志的描述串（调用方各自映射响应类型）。
pub(crate) async fn ensure_workspace_via_dev(
    addr: &str,
    app_id: &str,
    user_id: &str,
) -> Result<(), String> {
    let resp = crate::http_client::shared_client()
        .post(format!("{addr}/api/userapp/ensure-workspace"))
        .json(&serde_json::json!({"appId": app_id, "userId": user_id}))
        .send()
        .await
        .map_err(|e| format!("dev container ensure-workspace failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("ensure-workspace returned {status}: {text}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::workspace::CreateWorkspaceBody;

    #[test]
    fn create_workspace_body_is_camel_case() {
        let raw = serde_json::json!({"appId": "app-1", "userId": "u1"});
        let body: CreateWorkspaceBody = serde_json::from_value(raw).expect("deserialize");
        assert_eq!(body.app_id, "app-1");
        assert_eq!(body.user_id, "u1");
    }
}
