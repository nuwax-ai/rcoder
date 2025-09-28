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
///
/// ## 📨 支持的消息类型
///
/// 返回的UnifiedSessionMessage包含以下主要类型：
///
/// 1. **SessionPromptStart** - 用户发送prompt开始通知
/// 2. **SessionPromptEnd** - Agent执行结束通知（包含5种停止原因：EndTurn, MaxTokens, MaxTurnRequests, Refusal, Cancelled）
/// 3. **AgentSessionUpdate** - Agent执行过程中的更新通知（包含8种子类型：UserMessageChunk, AgentMessageChunk, AgentThoughtChunk, ToolCall, ToolCallUpdate, Plan, AvailableCommandsUpdate, CurrentModeUpdate）
/// 4. **Heartbeat** - SSE连接心跳消息
///
/// ## 🔄 事件类型映射
///
/// SSE消息事件名称与UnifiedSessionMessage类型的映射关系：
/// - `prompt_start` → SessionMessageType::SessionPromptStart
/// - `prompt_end` → SessionMessageType::SessionPromptEnd
/// - `user_message_chunk` → SessionMessageType::AgentSessionUpdate (sub_type: "user_message_chunk")
/// - `agent_message_chunk` → SessionMessageType::AgentSessionUpdate (sub_type: "agent_message_chunk")
/// - `agent_thought_chunk` → SessionMessageType::AgentSessionUpdate (sub_type: "agent_thought_chunk")
/// - `tool_call` → SessionMessageType::AgentSessionUpdate (sub_type: "tool_call")
/// - `tool_call_update` → SessionMessageType::AgentSessionUpdate (sub_type: "tool_call_update")
/// - `plan` → SessionMessageType::AgentSessionUpdate (sub_type: "plan")
/// - `available_commands_update` → SessionMessageType::AgentSessionUpdate (sub_type: "available_commands_update")
/// - `current_mode_update` → SessionMessageType::AgentSessionUpdate (sub_type: "current_mode_update")
/// - `heartbeat` → SessionMessageType::Heartbeat
///
/// ## 💡 前端集成建议
///
/// 前端开发者需要：
/// 1. 建立SSE连接并监听不同的事件类型
/// 2. 根据message_type和sub_type处理不同的消息场景
/// 3. 实现心跳检测机制确保连接活跃
/// 4. 处理错误情况并实现自动重连
///
/// 详细的JSON格式示例请参考UnifiedSessionMessage结构体定义和具体字段的Schema说明。
#[utoipa::path(
    get,
    path = "/agent/progress/{session_id}",
    params(
        SessionNotificationParams
    ),
    responses(
        (
            status = 200,
            description = "成功建立SSE连接，开始推送实时更新。连接建立后，将实时推送该会话的所有状态更新消息。",
            content_type = "text/event-stream",
            body = UnifiedSessionMessage,
            examples(
                ("SessionPromptStart" = (summary = "用户请求开始", value = json!({
                    "session_id": "session456",
                    "message_type": "SessionPromptStart",
                    "sub_type": "prompt_start",
                    "data": {
                        "type": "prompt_start",
                        "prompt": "帮我写一个Rust的Hello World程序",
                        "attachments": [
                            {
                                "type": "text",
                                "content": "请包含详细的注释说明"
                            }
                        ],
                        "user_id": "user123",
                        "project_id": "test_project"
                    },
                    "timestamp": "2023-12-01T10:30:00Z"
                }))),
                ("SessionPromptEnd_EndTurn" = (summary = "正常结束", value = json!({
                    "session_id": "session456",
                    "message_type": "SessionPromptEnd",
                    "sub_type": "end_turn",
                    "data": {
                        "stop_reason": "end_turn",
                        "message": "任务完成：成功创建Hello World程序",
                        "tool_calls": [
                            {
                                "name": "write_file",
                                "status": "completed",
                                "duration_ms": 150
                            }
                        ],
                        "total_tokens": 245,
                        "duration_ms": 3200
                    },
                    "timestamp": "2023-12-01T10:30:05Z"
                }))),
                ("SessionPromptEnd_MaxTokens" = (summary = "令牌限制", value = json!({
                    "session_id": "session456",
                    "message_type": "SessionPromptEnd",
                    "sub_type": "max_tokens",
                    "data": {
                        "stop_reason": "max_tokens",
                        "message": "已达到最大令牌数限制",
                        "error_message": "Token limit exceeded: max_tokens=4000, used_tokens=4025",
                        "suggestion": "请简化请求或分段处理"
                    },
                    "timestamp": "2023-12-01T10:30:45Z"
                }))),
                ("AgentMessageChunk" = (summary = "Agent响应消息", value = json!({
                    "session_id": "session456",
                    "message_type": "AgentSessionUpdate",
                    "sub_type": "agent_message_chunk",
                    "data": {
                        "content": {
                            "type": "text",
                            "text": "当然可以！以下是一个简单的Rust Hello World程序：\\n\\n```rust\\nfn main() {\\n    println!(\\\"Hello, World!\\\");\\n}\\n```",
                            "annotations": null,
                            "meta": null
                        },
                        "is_final": false
                    },
                    "timestamp": "2023-12-01T10:30:02Z"
                }))),
                ("Heartbeat" = (summary = "心跳消息", value = json!({
                    "session_id": "session456",
                    "message_type": "Heartbeat",
                    "sub_type": "ping",
                    "data": {
                        "type": "heartbeat",
                        "message": "keep-alive",
                        "timestamp": "2023-12-01T10:31:00Z"
                    },
                    "timestamp": "2023-12-01T10:31:00Z"
                })))
            )
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
    description = "通过SSE协议建立与指定会话的实时通信连接，推送AI代理执行进度更新。\n\n## 🎯 前端对接指南\n\n### 🔌 连接建立\n前端通过此接口建立SSE连接后，会实时接收该会话的所有状态更新消息。\n\n### 📨 消息格式\n每条消息都是标准的SSE格式，包含事件类型和数据：\n```javascript\n// SSE消息格式\nevent: [事件类型]\ndata: [JSON格式的UnifiedSessionMessage]\n\n// 例如：\nevent: prompt_start\ndata: {\"session_id\":\"session456\",\"message_type\":\"SessionPromptStart\",\"sub_type\":\"prompt_start\",\"data\":{},\"timestamp\":\"2023-12-01T10:30:00Z\"}\n```\n\n### 🔄 事件类型映射\n- `prompt_start`: SessionPromptStart消息\n- `prompt_end`: SessionPromptEnd消息  \n- `user_message_chunk`: 用户消息块\n- `agent_message_chunk`: Agent响应消息块\n- `agent_thought_chunk`: Agent思考过程\n- `tool_call`: 工具调用通知\n- `tool_call_update`: 工具调用状态更新\n- `available_commands_update`: 可用命令更新\n- `heartbeat`: 心跳消息\n\n## 📋 UnifiedSessionMessage 完整场景示例\n\n### 🚀 SessionPromptStart（用户请求开始）\n```json\n{\n  \"session_id\": \"session456\",\n  \"message_type\": \"SessionPromptStart\",\n  \"sub_type\": \"prompt_start\",\n  \"data\": {\n    \"type\": \"prompt_start\",\n    \"prompt\": \"帮我写一个Rust的Hello World程序\",\n    \"attachments\": [\n      {\n        \"type\": \"text\",\n        \"content\": \"这是附加的代码要求\"\n      }\n    ],\n    \"user_id\": \"user123\",\n    \"project_id\": \"test_project\"\n  },\n  \"timestamp\": \"2023-12-01T10:30:00Z\"\n}\n```\n\n### 🔄 AgentSessionUpdate（执行过程更新）\n\n#### Agent思考过程\n```json\n{\n  \"session_id\": \"session456\",\n  \"message_type\": \"AgentSessionUpdate\",\n  \"sub_type\": \"agent_thought_chunk\",\n  \"data\": {\n    \"thinking\": \"用户要求写一个Hello World程序，我需要创建main.rs文件并包含基本的println!宏调用。\",\n    \"confidence\": 0.95\n  },\n  \"timestamp\": \"2023-12-01T10:30:01Z\"\n}\n```\n\n#### Agent文本响应\n```json\n{\n  \"session_id\": \"session456\",\n  \"message_type\": \"AgentSessionUpdate\",\n  \"sub_type\": \"agent_message_chunk\",\n  \"data\": {\n    \"content\": {\n      \"type\": \"text\",\n      \"text\": \"当然可以！以下是一个简单的Rust Hello World程序：\\n\\n```rust\\nfn main() {\\n    println!(\\\"Hello, World!\\\");\\n}\\n```\"\n    },\n    \"is_final\": false\n  },\n  \"timestamp\": \"2023-12-01T10:30:02Z\"\n}\n```\n\n#### 工具调用\n```json\n{\n  \"session_id\": \"session456\",\n  \"message_type\": \"AgentSessionUpdate\",\n  \"sub_type\": \"tool_call\",\n  \"data\": {\n    \"tool_call\": {\n      \"name\": \"write_file\",\n      \"arguments\": {\n        \"path\": \"src/main.rs\",\n        \"content\": \"fn main() {\\n    println!(\\\"Hello, World!\\\");\\n}\"\n      },\n      \"tool_call_id\": \"call_123456\"\n    },\n    \"status\": \"started\"\n  },\n  \"timestamp\": \"2023-12-01T10:30:03Z\"\n}\n```\n\n#### 工具调用更新\n```json\n{\n  \"session_id\": \"session456\",\n  \"message_type\": \"AgentSessionUpdate\",\n  \"sub_type\": \"tool_call_update\",\n  \"data\": {\n    \"tool_call_id\": \"call_123456\",\n    \"result\": {\n      \"status\": \"success\",\n      \"output\": {\n        \"path\": \"src/main.rs\",\n        \"content_length\": 48,\n        \"created\": true\n      }\n    }\n  },\n  \"timestamp\": \"2023-12-01T10:30:04Z\"\n}\n```\n\n### 🛑 SessionPromptEnd（执行结束）\n\n#### 正常结束\n```json\n{\n  \"session_id\": \"session456\",\n  \"message_type\": \"SessionPromptEnd\",\n  \"sub_type\": \"end_turn\",\n  \"data\": {\n    \"stop_reason\": \"end_turn\",\n    \"message\": \"成功创建了Hello World程序\",\n    \"tool_calls\": [\n      {\n        \"name\": \"write_file\",\n        \"status\": \"completed\",\n        \"duration_ms\": 150\n      }\n    ],\n    \"total_tokens\": 245,\n    \"duration_ms\": 3200\n  },\n  \"timestamp\": \"2023-12-01T10:30:05Z\"\n}\n```\n\n#### 令牌限制错误\n```json\n{\n  \"session_id\": \"session456\",\n  \"message_type\": \"SessionPromptEnd\",\n  \"sub_type\": \"max_tokens\",\n  \"data\": {\n    \"stop_reason\": \"max_tokens\",\n    \"message\": \"已达到最大令牌数限制\",\n    \"error_message\": \"Token limit exceeded: max_tokens=4000, used_tokens=4025\",\n    \"suggestion\": \"请简化请求或分段处理\"\n  },\n  \"timestamp\": \"2023-12-01T10:30:45Z\"\n}\n```\n\n#### 用户取消\n```json\n{\n  \"session_id\": \"session456\",\n  \"message_type\": \"SessionPromptEnd\",\n  \"sub_type\": \"cancelled\",\n  \"data\": {\n    \"stop_reason\": \"cancelled\",\n    \"message\": \"用户取消了请求\",\n    \"error_message\": \"Request cancelled by user\",\n    \"progress\": 65\n  },\n  \"timestamp\": \"2023-12-01T10:30:05Z\"\n}\n```\n\n### 💓 Heartbeat（心跳消息）\n```json\n{\n  \"session_id\": \"session456\",\n  \"message_type\": \"Heartbeat\",\n  \"sub_type\": \"ping\",\n  \"data\": {\n    \"type\": \"heartbeat\",\n    \"message\": \"keep-alive\",\n    \"timestamp\": \"2023-12-01T10:31:00Z\"\n  },\n  \"timestamp\": \"2023-12-01T10:31:00Z\"\n}\n```\n\n### 📊 典型完整流程示例\n1. **SessionPromptStart** → 用户发送prompt\n2. **AgentSessionUpdate** → 多种更新消息流（思考→响应→工具调用→工具结果）\n3. **SessionPromptEnd** → 执行完成状态\n4. **Heartbeat** → 定期保持连接\n\n## 💡 前端开发建议\n1. **连接管理**: 实现自动重连机制\n2. **消息队列**: 对消息进行队列处理，避免阻塞UI\n3. **心跳检测**: 定期检查心跳消息，确保连接活跃\n4. **错误处理**: 监听连接错误和SessionPromptEnd中的错误信息\n5. **状态同步**: 根据消息类型和子类型更新UI状态"
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
                let messages: Vec<UnifiedSessionMessage> = if let Some(session_data) = SESSION_CACHE.get(&session_id_clone) {
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
