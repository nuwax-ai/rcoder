//! Computer Agent Progress Handler
//!
//! 处理 GET /computer/progress/{session_id} 请求 (SSE 流)

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::stream::Stream;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::StreamExt;
use tracing::{error, info, warn};

use crate::http_server::router::AppState;
use crate::service::SESSION_CACHE;
use shared_types::HttpResult;

/// Computer Agent 进度流 (SSE)
///
/// 直接从 SESSION_CACHE 订阅消息流,无需 gRPC
#[utoipa::path(
    get,
    path = "/computer/progress/{session_id}",
    responses(
        (status = 200, description = "SSE progress stream", content_type = "text/event-stream"),
        (status = 404, description = "Session not found"),
        (status = 500, description = "Internal server error")
    ),
    tag = "Computer Agent"
)]
pub async fn handle_computer_progress(
    State(_state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<
    Sse<impl Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<HttpResult<String>>),
> {
    info!(
        "📡 [HTTP] Computer Agent 进度流订阅: session_id={}",
        session_id
    );

    // 1. 从 SESSION_CACHE 获取 session_data
    let session_data = match SESSION_CACHE.get(&session_id) {
        Some(data) => data,
        None => {
            warn!("⚠️  [HTTP] Session 不存在: session_id={}", session_id);
            return Err((
                StatusCode::NOT_FOUND,
                Json(HttpResult::error(
                    "SESSION_NOT_FOUND",
                    &format!("Session {} not found", session_id),
                )),
            ));
        }
    };

    // 2. 创建新的消息订阅
    let (message_rx, _cancel_token) = match session_data.create_new_connection(1000).await {
        Ok(conn) => conn,
        Err(e) => {
            error!(
                "❌ [HTTP] 创建 session 连接失败: session_id={}, error={}",
                session_id, e
            );
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(HttpResult::error(
                    "INTERNAL_ERROR",
                    &format!("Failed to create session connection: {}", e),
                )),
            ));
        }
    };

    // 3. 将 UnifiedSessionMessage 转换为 SSE Event 流
    let stream = tokio_stream::wrappers::ReceiverStream::new(message_rx).map(|msg| {
        // 将消息序列化为 JSON
        let json_str = match serde_json::to_string(&msg) {
            Ok(s) => s,
            Err(e) => {
                error!("❌ [HTTP] 消息序列化失败: {}", e);
                return Ok(Event::default().data("{}"));
            }
        };

        // 根据消息类型设置 SSE event 名称
        let event_name = format!("{:?}", msg.message_type);

        Ok(Event::default().event(event_name).data(json_str))
    });

    info!("✅ [HTTP] SSE 流已建立: session_id={}", session_id);

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    ))
}
