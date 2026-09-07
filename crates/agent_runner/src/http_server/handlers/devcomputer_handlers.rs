//! DevComputer 委托 Handlers
//!
//! 处理 /devcomputer/* 路由（除 chat 外）。
//!
//! 设计原则：
//! 1. 共享容器：/devcomputer 和 /computer 使用同一个 user_id 容器
//! 2. 零逻辑分歧：路由直接指向 /computer/* handler，不复制业务逻辑
//! 3. 差异仅在 /devcomputer/chat 的 auto_reload 配置注入（见 devcomputer_chat.rs）
//!
//! progress 之所以仍需一个独立薄 handler（而非在 router.rs 里直接复用
//! `computer_progress::handle_computer_progress`），是因为 utoipa 的路径取自
//! `#[utoipa::path]` 属性——直接复用会让 `/devcomputer/progress/{session_id}`
//! 从 OpenAPI 文档里消失。流式逻辑本身仍在 [`super::progress_sse`]，无副本。

use axum::{
    Json,
    extract::Path,
    http::{HeaderMap, StatusCode},
    response::sse::Sse,
};
use shared_types::HttpResult;
use tracing::info;

use super::progress_sse::{SseStream, progress_sse};

/// 与 /computer/progress 同前缀（两者共享容器与逻辑，日志上不做区分）
const LOG_PREFIX: &str = "HTTP";

/// GET /devcomputer/progress/{session_id} — 与 /computer/progress 同源
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
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Sse<SseStream>, (StatusCode, Json<HttpResult<String>>)> {
    // 订阅日志由各薄 handler 自己打（progress_sse 内部不打）。此处文本与重构前
    // 委托 handle_computer_progress 时完全一致，运维 grep 不受影响。
    info!(
        "📡 [{}] Computer Agent progress stream subscribed: session_id={}",
        LOG_PREFIX, session_id
    );
    progress_sse(headers, session_id, LOG_PREFIX).await
}
