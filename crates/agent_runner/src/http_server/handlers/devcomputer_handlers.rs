//! DevComputer 委托 Handlers
//!
//! 处理 /devcomputer/* 路由（除 chat 外）。
//!
//! 设计原则：
//! 1. 共享容器：/devcomputer 和 /computer 使用同一个 user_id 容器
//! 2. 零逻辑分歧：路由直接指向 /computer/* handler，不复制业务逻辑
//! 3. 差异仅在 /devcomputer/chat 的 auto_reload 配置注入（见 devcomputer_chat.rs）
//!
//! 本文件仅保留需要返回类型适配的 handler（progress），
//! 其余路由在 router.rs 中直接复用 computer_* handler。

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::IntoResponse,
};
use std::sync::Arc;

use crate::http_server::router::AppState;

use super::computer_progress::handle_computer_progress;

/// GET /devcomputer/progress/{session_id} — 委托到 computer_progress
///
/// 需要独立 handler 因为 `SseStream` 是 computer_progress 模块的私有类型别名，
/// 无法在 router 中直接引用返回类型。
#[utoipa::path(
    get,
    path = "/devcomputer/progress/{session_id}",
    params(
        ("session_id" = String, Path, description = "会话ID")
    ),
    responses(
        (status = 200, description = "SSE progress stream", content_type = "text/event-stream"),
        (status = 404, description = "Session not found"),
        (status = 500, description = "Internal server error")
    ),
    tag = "DevComputer"
)]
pub async fn devcomputer_progress(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    handle_computer_progress(state, headers, Path(session_id)).await
}
