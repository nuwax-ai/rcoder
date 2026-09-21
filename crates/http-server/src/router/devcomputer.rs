//! DevComputer 调试路由 — 委托给 /computer/* 处理器，共享同一个容器（从 router.rs 拆出）。

use std::sync::Arc;

use crate::router::AppState;
use axum::Router;
use axum::routing::{get, post};

use crate::handler;

pub(super) fn devcomputer_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/devcomputer/chat", post(handler::handle_devcomputer_chat))
        .route(
            "/devcomputer/agent/stop",
            post(handler::devcomputer_agent_stop),
        )
        .route(
            "/devcomputer/agent/status",
            post(handler::devcomputer_agent_status),
        )
        .route(
            "/devcomputer/agent/session/cancel",
            post(handler::devcomputer_agent_session_cancel),
        )
        .route(
            "/devcomputer/notify-resolved",
            post(handler::devcomputer_notify_resolved),
        )
        .route(
            "/devcomputer/progress/{session_id}",
            get(handler::devcomputer_agent_progress_notification),
        )
        .with_state(state)
}
