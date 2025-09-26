//! Agent 执行任务的时候，session_notification 的通知消息
//!
//! 通过SSE协议将SessionUpdate消息实时推送给前端

use axum::{
    extract::Path,
    response::sse::{Event, Sse},
};
use futures::stream::{self, Stream};
use std::{convert::Infallible, time::Duration};
use tokio::time::sleep;
use tracing::{info, debug};
use serde::Serialize;
use utoipa::{IntoParams, ToSchema};
use crate::model::HttpResult;

/// SSE 事件响应结构
#[derive(Debug, Serialize, ToSchema)]
pub struct SessionUpdateEvent {
    /// 事件类型
    pub event_type: String,
    /// 会话ID
    pub session_id: String,
    /// 消息内容
    pub message: String,
    /// 时间戳
    pub timestamp: String,
}

/// 会话通知路径参数
#[derive(Debug, IntoParams)]
pub struct SessionNotificationParams {
    /// 会话ID，用于标识特定的会话连接
    #[param(example = "session456")]
    pub session_id: String,
}

/// 建立SSE连接，实时推送该session的SessionUpdate消息
///
/// 通过Server-Sent Events (SSE)协议实时推送AI代理执行进度和状态更新
#[utoipa::path(
    get,
    path = "/agent/progress/{session_id}",
    params(
        SessionNotificationParams
    ),
    responses(
        (
            status = 200,
            description = "成功建立SSE连接，开始推送实时更新",
            content_type = "text/event-stream",
            body = SessionUpdateEvent,
            example = json!({
                "event_type": "session_update",
                "session_id": "session456",
                "message": "Agent正在处理您的请求...",
                "timestamp": "2023-12-01T10:30:00Z"
            })
        ),
        (
            status = 400,
            description = "无效的会话ID",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "INVALID_SESSION",
                    "message": "Invalid session ID"
                }
            })
        ),
        (
            status = 404,
            description = "会话不存在",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "SESSION_NOT_FOUND",
                    "message": "Session not found"
                }
            })
        )
    ),
    tag = "agent",
    operation_id = "agent_session_notification",
    summary = "建立Agent会话通知连接",
    description = "通过SSE协议建立与指定会话的实时通信连接，推送AI代理执行进度更新"
)]
pub async fn agent_session_notification(
    Path(session_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, crate::model::AppError> {

    info!("🔌 SSE连接建立: session_id={}", session_id);

    // 创建SSE流
    let stream = stream::unfold(session_id.clone(), move |session_id| {
        let session_id_clone = session_id.clone();
        async move {
            loop {
                // 获取并清空该session的消息
                let messages = crate::service::drain_session_messages(&session_id_clone
                );

                if !messages.is_empty() {
                    debug!("📤 推送 {} 条消息到 session: {}", messages.len(), session_id_clone);

                    // 逐条发送消息
                    for msg in messages {
                        let event = Event::default()
                            .event("session_update")
                            .data(serde_json::to_string(&msg).unwrap_or_else(|_| "{}".to_string()));

                        return Some((Ok(event), session_id_clone));
                    }
                }

                // 没有消息，等待一段时间再检查
                sleep(Duration::from_millis(100)).await;

                // 发送心跳保持连接（每30秒一次）
                // 注意：这里简化处理，实际可以用更复杂的心跳逻辑
                // 暂时通过定期重试来保持连接
            }
        }
    });

    Ok(Sse::new(stream))
}