//! RCoder Agent Progress Handler
//!
//! 处理 GET /agent/progress/{session_id} 请求 (SSE 流)
//!
//! 流式逻辑见 [`super::progress_sse`]——与 `/computer/progress` 和
//! `/devcomputer/progress` 完全同源，本文件只保留路由专属的 utoipa 元数据与日志前缀。

use axum::{
    Json,
    extract::Path,
    http::{HeaderMap, StatusCode},
    response::sse::Sse,
};
use shared_types::HttpResult;
use tracing::info;

use super::progress_sse::{SseStream, progress_sse};

/// 本路由的日志前缀（运维按前缀 grep，不得随重构改变）
const LOG_PREFIX: &str = "RCoder";

/// RCoder Agent 进度流 (SSE)
///
/// 直接从 SESSION_CACHE 订阅消息流，无需 gRPC
#[utoipa::path(
    get,
    path = "/agent/progress/{session_id}",
    params(
        ("session_id" = String, Path, description = "会话ID")
    ),
    responses(
        (status = 200, description = "SSE progress stream", content_type = "text/event-stream"),
        (status = 404, description = "Session not found"),
        (status = 500, description = "Internal server error")
    ),
    tag = "RCoder Agent"
)]
pub async fn handle_rcoder_progress(
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Sse<SseStream>, (StatusCode, Json<HttpResult<String>>)> {
    info!(
        "📡 [{}] Progress stream subscribed: session_id={}",
        LOG_PREFIX, session_id
    );
    progress_sse(headers, session_id, LOG_PREFIX).await
}
