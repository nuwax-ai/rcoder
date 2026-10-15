//! chat / agent 会话与通知域路由（从 router.rs 拆出）。

use std::sync::Arc;

use crate::router::AppState;
use axum::Router;
use axum::routing::{get, post};

use crate::handler;

pub(super) fn api_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/chat", post(handler::handle_chat))
        // Axum SSE 代理处理器，直接返回 SSE 流
        .route(
            "/agent/progress/{session_id}",
            get(handler::agent_session_notification),
        )
        .route("/agent/session/cancel", post(handler::agent_session_cancel))
        .route(
            "/agent/notify-resolved",
            post(handler::agent_notify_resolved),
        )
        .route("/agent/stop", post(handler::agent_stop))
        .route("/agent/status/{project_id}", get(handler::agent_status))
        .with_state(state)
}
