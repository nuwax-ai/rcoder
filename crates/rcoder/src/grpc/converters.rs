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
/// 简化版：直接使用透传的字段
pub fn from_grpc_progress_event(
    event: ProgressEvent,
    session_id: &str,
) -> Option<UnifiedSessionMessage> {
    use chrono::Utc;
    use shared_types::SessionMessageType;

    let timestamp = chrono::DateTime::from_timestamp_millis(event.timestamp)
        .unwrap_or_else(|| Utc::now());

    // 从 message_type 字符串解析枚举
    let message_type = match event.message_type.as_str() {
        "SessionPromptStart" => SessionMessageType::SessionPromptStart,
        "SessionPromptEnd" => SessionMessageType::SessionPromptEnd,
        "AgentSessionUpdate" => SessionMessageType::AgentSessionUpdate,
        "Heartbeat" => SessionMessageType::Heartbeat,
        _ => SessionMessageType::AgentSessionUpdate, // 默认为 AgentSessionUpdate
    };

    // 直接解析 payload JSON
    let data = serde_json::from_str(&event.payload)
        .unwrap_or_else(|_| serde_json::json!({}));

    Some(UnifiedSessionMessage {
        session_id: session_id.to_string(),
        message_type,
        sub_type: event.sub_type,
        data,
        timestamp,
    })
}
