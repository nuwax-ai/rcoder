//! 类型转换器
//!
//! 在 Rust 内部类型和 gRPC Protobuf 类型之间进行转换

use shared_types::grpc::{
    attachment, attachment_source, Attachment as GrpcAttachment,
    AttachmentSource as GrpcAttachmentSource, AudioAttachment as GrpcAudioAttachment, Base64Data,
    ChatAgentConfig as GrpcChatAgentConfig, ChatAgentServerConfig as GrpcChatAgentServerConfig,
    ChatContextServerConfig as GrpcChatContextServerConfig, ChatRequest as GrpcChatRequest,
    ChatResponse as GrpcChatResponse, DocumentAttachment as GrpcDocumentAttachment,
    ImageAttachment as GrpcImageAttachment, ImageDimensions as GrpcImageDimensions,
    ModelEnvBinding as GrpcModelEnvBinding, ModelEnvBindingSource as GrpcModelEnvBindingSource,
    ModelProviderConfig as GrpcModelProviderConfig, ProgressEvent,
    TextAttachment as GrpcTextAttachment,
};
use shared_types::{Attachment, AttachmentSource, ModelProviderConfig, UnifiedSessionMessage};
use shared_types::{
    ChatAgentConfig, ChatAgentServerConfig, ChatContextServerConfig, ModelEnvBinding,
    ModelEnvBindingSource,
};

/// 将内部 ChatRequest 转换为 gRPC ChatRequest
///
/// 注意：此函数目前未被使用，chat_client.rs 直接构建 GrpcChatRequest。
/// 保留以备将来使用或重构。
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)] // 直接映射 ChatRequest 字段
pub fn to_grpc_chat_request(
    project_id: String,
    session_id: String,
    prompt: String,
    attachments: Vec<Attachment>,
    data_source_attachments: Vec<String>,
    model_config: Option<ModelProviderConfig>,
    request_id: Option<String>,
    // 新增参数 (v2)
    system_prompt: Option<String>,
    user_prompt: Option<String>,
    agent_config: Option<ChatAgentConfig>,
    service_type: Option<shared_types::ServiceType>,
    user_id: Option<String>, // 新增：用于 ComputerAgentRunner 模式
) -> GrpcChatRequest {
    GrpcChatRequest {
        project_id,
        session_id,
        prompt,
        model_config: model_config.map(to_grpc_model_config),
        attachments: attachments.into_iter().map(to_grpc_attachment).collect(),
        request_id,
        data_source_attachments,
        // 新增字段 (v2)
        system_prompt,
        user_prompt,
        agent_config: agent_config.map(to_grpc_chat_agent_config),
        service_type: service_type.map(|st| format!("{:?}", st)),
        user_id, // 传递 user_id
    }
}

/// 将 ModelProviderConfig 转换为 gRPC 格式
pub fn to_grpc_model_config(config: ModelProviderConfig) -> GrpcModelProviderConfig {
    GrpcModelProviderConfig {
        id: config.id, // 保留原始 ID，用于会话复用判断
        provider: config.name,
        model: config.default_model,
        api_key: Some(config.api_key),
        api_base: Some(config.base_url),
        requires_openai_auth: Some(config.requires_openai_auth),
        api_protocol: config.api_protocol,
        wire_api: config.wire_api,
    }
}

/// 将 AttachmentSource 转换为 gRPC 格式
///
/// 辅助函数，将 Rust 侧的 AttachmentSource 枚举转换为 gRPC 的 AttachmentSource
fn to_grpc_attachment_source(source: AttachmentSource) -> Option<GrpcAttachmentSource> {
    let grpc_source = match source {
        AttachmentSource::FilePath { path } => attachment_source::Source::FilePath(path),
        AttachmentSource::Base64 { data, mime_type } => {
            attachment_source::Source::Base64(Base64Data { data, mime_type })
        }
        AttachmentSource::Url { url } => attachment_source::Source::Url(url),
    };

    Some(GrpcAttachmentSource {
        source: Some(grpc_source),
    })
}

/// 将 Attachment 转换为 gRPC 格式
///
/// 完整实现：根据 Attachment 类型正确填充 gRPC 的 oneof 字段
pub fn to_grpc_attachment(attachment: Attachment) -> GrpcAttachment {
    let attachment_type = match attachment {
        Attachment::Text(text) => attachment::AttachmentType::Text(GrpcTextAttachment {
            id: text.id,
            source: to_grpc_attachment_source(text.source),
            filename: text.filename,
            description: text.description,
        }),
        Attachment::Image(image) => attachment::AttachmentType::Image(GrpcImageAttachment {
            id: image.id,
            source: to_grpc_attachment_source(image.source),
            mime_type: image.mime_type,
            filename: image.filename,
            description: image.description,
            dimensions: image.dimensions.map(|d| GrpcImageDimensions {
                width: d.width,
                height: d.height,
            }),
        }),
        Attachment::Audio(audio) => attachment::AttachmentType::Audio(GrpcAudioAttachment {
            id: audio.id,
            source: to_grpc_attachment_source(audio.source),
            mime_type: audio.mime_type,
            filename: audio.filename,
            description: audio.description,
            duration: audio.duration,
        }),
        Attachment::Document(doc) => attachment::AttachmentType::Document(GrpcDocumentAttachment {
            id: doc.id,
            source: to_grpc_attachment_source(doc.source),
            mime_type: doc.mime_type,
            filename: doc.filename,
            description: doc.description,
            size: doc.size,
        }),
    };

    GrpcAttachment {
        attachment_type: Some(attachment_type),
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
    use tracing::warn;

    let timestamp = match chrono::DateTime::from_timestamp_millis(event.timestamp) {
        Some(ts) => ts,
        None => {
            warn!(
                "⚠️ [CONVERTER] Invalid timestamp: session_id={}, timestamp={}, using current time",
                session_id, event.timestamp
            );
            Utc::now()
        }
    };

    // 从 message_type 字符串解析枚举
    let message_type = match event.message_type.as_str() {
        "SessionPromptStart" => SessionMessageType::SessionPromptStart,
        "SessionPromptEnd" => SessionMessageType::SessionPromptEnd,
        "AgentSessionUpdate" => SessionMessageType::AgentSessionUpdate,
        "AcpRequestPermission" => SessionMessageType::AcpRequestPermission,
        "Heartbeat" => SessionMessageType::Heartbeat,
        _ => SessionMessageType::AgentSessionUpdate, // 默认为 AgentSessionUpdate
    };

    // 解析 payload JSON
    let data = match serde_json::from_str(&event.payload) {
        Ok(data) => data,
        Err(e) => {
            warn!(
                "⚠️ [CONVERTER] Failed to parse gRPC payload: session_id={}, payload_preview={}, error={}",
                session_id,
                event.payload.chars().take(100).collect::<String>(),
                e
            );
            // 返回包含原始 payload 的错误对象
            serde_json::json!({
                "_parse_error": e.to_string(),
                "_original_payload": event.payload
            })
        }
    };

    Some(UnifiedSessionMessage {
        session_id: session_id.to_string(),
        message_type,
        sub_type: event.sub_type,
        data,
        timestamp,
    })
}

// === ChatAgentConfig 类型转换 (v2) ===

/// 将 ChatAgentConfig 转换为 gRPC 格式
pub fn to_grpc_chat_agent_config(config: ChatAgentConfig) -> GrpcChatAgentConfig {
    GrpcChatAgentConfig {
        agent_server: config.agent_server.map(to_grpc_chat_agent_server_config),
        context_servers: config
            .context_servers
            .into_iter()
            .map(|(k, v)| (k, to_grpc_chat_context_server_config(v)))
            .collect(),
    }
}

/// 将 ChatAgentServerConfig 转换为 gRPC 格式
pub fn to_grpc_chat_agent_server_config(
    config: ChatAgentServerConfig,
) -> GrpcChatAgentServerConfig {
    GrpcChatAgentServerConfig {
        agent_id: config.agent_id,
        command: config.command,
        args: config.args.unwrap_or_default(),
        env: config.env.unwrap_or_default(),
        metadata: config.metadata.unwrap_or_default(),
        model_env_bindings: config
            .model_env_bindings
            .into_iter()
            .map(to_grpc_model_env_binding)
            .collect(),
        agent_mode: config.agent_mode,
    }
}

fn to_grpc_model_env_binding(binding: ModelEnvBinding) -> GrpcModelEnvBinding {
    GrpcModelEnvBinding {
        env_key: binding.env_key,
        source: to_grpc_model_env_binding_source(binding.source) as i32,
    }
}

fn to_grpc_model_env_binding_source(source: ModelEnvBindingSource) -> GrpcModelEnvBindingSource {
    match source {
        ModelEnvBindingSource::ApiKey => GrpcModelEnvBindingSource::ApiKey,
        ModelEnvBindingSource::BaseUrl => GrpcModelEnvBindingSource::BaseUrl,
        ModelEnvBindingSource::DefaultModel => GrpcModelEnvBindingSource::DefaultModel,
        ModelEnvBindingSource::ProviderName => GrpcModelEnvBindingSource::ProviderName,
    }
}

/// 将 ChatContextServerConfig 转换为 gRPC 格式
pub fn to_grpc_chat_context_server_config(
    config: ChatContextServerConfig,
) -> GrpcChatContextServerConfig {
    GrpcChatContextServerConfig {
        source: config.source,
        enabled: config.enabled,
        command: config.command,
        args: config.args.unwrap_or_default(),
        env: config.env.unwrap_or_default(),
    }
}
