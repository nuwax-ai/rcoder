//! Agent 执行任务的时候，session_notification 的通知消息
//!
//! 通过SSE协议将UnifiedSessionMessage消息实时推送给前端

use crate::{AppError, model::HttpResult, model::UnifiedSessionMessage, service::SESSION_CACHE};
use axum::{
    extract::Path,
    response::sse::{Event, Sse},
};
use futures::stream::{self, Stream};
use serde::Serialize;
use std::{convert::Infallible, time::Duration};
use tokio::time::sleep;
use tracing::{debug, info};
use serde::Deserialize;
use utoipa::{IntoParams, ToSchema};

/// SSE 事件响应结构
#[derive(Debug, Serialize, ToSchema)]
pub struct SessionUpdateEvent {
    /// 事件类型
    pub event_type: String,
    /// 会话ID
    pub session_id: String,
    /// 统一会话消息
    pub message: UnifiedSessionMessage,
}

/// 会话通知路径参数
#[derive(Debug, Deserialize, IntoParams)]
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
            body = UnifiedSessionMessage,
            example = json!({
                "session_id": "session456",
                "message_type": "SessionPromptStart",
                "sub_type": "prompt_start",
                "data": {
                    "session_id": "session456"
                },
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
    description = "通过SSE协议建立与指定会话的实时通信连接，推送AI代理执行进度更新。\n\n## 消息类型说明\n\n### SessionMessageType（消息主类型）\n- `SessionPromptStart`: 用户发送prompt开始\n- `SessionPromptEnd`: Agent执行结束\n- `AgentSessionUpdate`: Agent执行过程中的更新\n- `Heartbeat`: SSE连接心跳消息\n\n### SessionPromptEnd sub_type（结束原因）\n- `EndTurn`: 正常结束\n- `MaxTokens`: 达到最大令牌数限制\n- `MaxTurnRequests`: 达到最大请求数限制\n- `Refusal`: 代理拒绝继续\n- `Cancelled`: 用户取消\n\n### AgentSessionUpdate sub_type（会话更新类型）\n- `UserMessageChunk`: 用户消息块\n- `AgentMessageChunk`: Agent响应消息块\n- `AgentThoughtChunk`: Agent思考过程消息块\n- `ToolCall`: 工具调用通知\n- `ToolCallUpdate`: 工具调用状态更新\n- `AvailableCommandsUpdate`: 可用命令更新\n\n### ContentBlock type（内容块类型）\n- `text`: 纯文本内容\n- `image`: 图片内容\n- `audio`: 音频内容\n- `resource_link`: 资源链接\n- `resource`: 嵌入式资源内容"
)]
pub async fn agent_session_notification(
    Path(params): Path<SessionNotificationParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    info!("🔌 SSE连接建立: session_id={}", params.session_id);

    // 创建SSE流
    let stream = stream::unfold(params.session_id.clone(), move |session_id| {
        let session_id_clone = session_id.clone();
        async move {
            loop {
                // 获取并清空该session的消息
                let messages = if let Some(session_data) = SESSION_CACHE.get(&session_id_clone) {
                    session_data.drain_messages()
                } else {
                    Vec::new()
                };

                if !messages.is_empty() {
                    debug!(
                        "📤 推送 {} 条消息到 session: {}",
                        messages.len(),
                        session_id_clone
                    );

                    // 逐条发送消息
                    for msg in messages {
                        // 根据消息类型动态设置事件名称
                        let event_name = match msg.message_type {
                            crate::model::SessionMessageType::SessionPromptStart => "prompt_start",
                            crate::model::SessionMessageType::SessionPromptEnd => "prompt_end",
                            crate::model::SessionMessageType::AgentSessionUpdate => &msg.sub_type,
                            crate::model::SessionMessageType::Heartbeat => "heartbeat",
                        };

                        let event: Event = Event::default()
                            .event(event_name)
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
