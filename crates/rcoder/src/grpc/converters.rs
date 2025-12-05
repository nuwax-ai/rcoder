//! 类型转换器
//!
//! 在 Rust 内部类型和 gRPC Protobuf 类型之间进行转换

use shared_types::grpc::{
    Attachment as GrpcAttachment, ChatRequest as GrpcChatRequest, ChatResponse as GrpcChatResponse,
    ModelProviderConfig as GrpcModelProviderConfig, ProgressEvent,
};
use shared_types::{Attachment, ModelProviderConfig, UnifiedSessionMessage};

/// 将内部 ChatRequest 转换为 gRPC ChatRequest
pub fn to_grpc_chat_request(
    project_id: String,
    session_id: String,
    prompt: String,
    attachments: Vec<Attachment>,
    data_source_attachments: Vec<String>,
    model_config: Option<ModelProviderConfig>,
    request_id: Option<String>,
) -> GrpcChatRequest {
    GrpcChatRequest {
        project_id,
        session_id,
        prompt,
        model_config: model_config.map(to_grpc_model_config),
        attachments: attachments.into_iter().map(to_grpc_attachment).collect(),
        request_id,
        data_source_attachments,
    }
}

/// 将 ModelProviderConfig 转换为 gRPC 格式
pub fn to_grpc_model_config(config: ModelProviderConfig) -> GrpcModelProviderConfig {
    GrpcModelProviderConfig {
        provider: config.name,
        model: config.default_model,
        api_key: Some(config.api_key),
        api_base: Some(config.base_url),
    }
}

/// 将 Attachment 转换为 gRPC 格式
///
/// 目前简化处理，仅传输基本信息
/// TODO: 实现完整的 Attachment 类型转换
pub fn to_grpc_attachment(_attachment: Attachment) -> GrpcAttachment {
    // 简化版本：暂时返回空附件
    // 完整实现需要根据 attachment 类型填充 oneof 字段
    GrpcAttachment {
        attachment_type: None,
    }
}

/// 从 gRPC ChatResponse 提取结果
pub fn from_grpc_chat_response(resp: GrpcChatResponse) -> (String, String, bool, Option<String>) {
    (resp.project_id, resp.session_id, resp.success, resp.error)
}

/// 从 gRPC ProgressEvent 转换为 UnifiedSessionMessage
///
/// 使用 oneof event 字段进行类型安全的转换
pub fn from_grpc_progress_event(
    event: ProgressEvent,
    session_id: &str,
) -> Option<UnifiedSessionMessage> {
    use chrono::Utc;
    use shared_types::grpc::progress_event::Event;
    use shared_types::SessionMessageType;

    let timestamp = chrono::DateTime::from_timestamp_millis(event.timestamp)
        .unwrap_or_else(|| Utc::now());

    let event_data = event.event?;

    let (message_type, sub_type, data) = match event_data {
        // 思考事件 → AgentSessionUpdate + agent_thought_chunk
        Event::Thinking(thinking) => {
            let data = serde_json::json!({
                "thinking": thinking.content,
                "is_complete": thinking.is_complete,
            });
            (
                SessionMessageType::AgentSessionUpdate,
                "agent_thought_chunk".to_string(),
                data,
            )
        }

        // 内容片段 → AgentSessionUpdate + agent_message_chunk
        Event::Chunk(chunk) => {
            let data = serde_json::json!({
                "content": {
                    "type": "text",
                    "text": chunk.content
                },
                "index": chunk.index,
                "is_final": false
            });
            (
                SessionMessageType::AgentSessionUpdate,
                "agent_message_chunk".to_string(),
                data,
            )
        }

        // 工具使用 → AgentSessionUpdate + tool_call/tool_call_update
        Event::ToolUse(tool) => {
            let (sub_type, data) = if tool.tool_output.is_some() {
                // 有输出 = 工具调用更新
                let data = serde_json::json!({
                    "tool_call_id": tool.tool_name,
                    "result": {
                        "status": if tool.is_error { "error" } else { "success" },
                        "output": serde_json::from_str::<serde_json::Value>(&tool.tool_output.unwrap_or_default()).ok()
                    }
                });
                ("tool_call_update".to_string(), data)
            } else {
                // 无输出 = 工具调用开始
                let data = serde_json::json!({
                    "tool_call": {
                        "name": tool.tool_name,
                        "arguments": serde_json::from_str::<serde_json::Value>(&tool.tool_input).ok(),
                    },
                    "status": "started"
                });
                ("tool_call".to_string(), data)
            };
            (SessionMessageType::AgentSessionUpdate, sub_type, data)
        }

        // 完成事件 → SessionPromptEnd + end_turn
        Event::Completion(completion) => {
            let data = serde_json::json!({
                "stop_reason": "end_turn",
                "message": completion.result,
                "total_tokens": completion.total_tokens,
                "duration_ms": completion.duration_ms
            });
            (
                SessionMessageType::SessionPromptEnd,
                "end_turn".to_string(),
                data,
            )
        }

        // 错误事件 → SessionPromptEnd + cancelled/max_tokens
        Event::Error(error) => {
            let data = serde_json::json!({
                "stop_reason": error.error_code.clone(),
                "error_message": error.error_message,
                "stack_trace": error.stack_trace
            });
            (
                SessionMessageType::SessionPromptEnd,
                error.error_code,
                data,
            )
        }

        // 日志事件 → AgentSessionUpdate + log
        Event::Log(log) => {
            let data = serde_json::json!({
                "level": log.level,
                "message": log.message
            });
            (
                SessionMessageType::AgentSessionUpdate,
                "log".to_string(),
                data,
            )
        }

        // AskConfirmation → AgentSessionUpdate + ask_confirmation
        Event::AskConfirmation(ask) => {
            let data = serde_json::json!({
                "message": ask.message,
                "options": ask.options,
                "default_option": ask.default_option
            });
            (
                SessionMessageType::AgentSessionUpdate,
                "ask_confirmation".to_string(),
                data,
            )
        }

        // ProgressNotification → AgentSessionUpdate + progress_notification
        Event::ProgressNotification(progress) => {
            let data = serde_json::json!({
                "status": progress.status,
                "percentage": progress.percentage,
                "details": progress.details
            });
            (
                SessionMessageType::AgentSessionUpdate,
                "progress_notification".to_string(),
                data,
            )
        }
    };

    Some(UnifiedSessionMessage {
        session_id: session_id.to_string(),
        message_type,
        sub_type,
        data,
        timestamp,
    })
}
