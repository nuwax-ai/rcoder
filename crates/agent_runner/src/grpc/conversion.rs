//! gRPC 类型与内部类型的转换函数

use shared_types::ModelProviderConfig;
use shared_types::grpc::{
    AutoReloadConfig as GrpcAutoReloadConfig,
    ChatAgentConfig as GrpcChatAgentConfig,
    ChatAgentServerConfig as GrpcChatAgentServerConfig,
    ChatContextServerConfig as GrpcChatContextServerConfig,
    ModelEnvBinding as GrpcModelEnvBinding, ModelEnvBindingSource as GrpcModelEnvBindingSource,
    ModelProviderConfig as GrpcModelProviderConfig,
    attachment, attachment_source,
};
use shared_types::{
    Attachment, AttachmentSource, AudioAttachment, AutoReloadConfig, DocumentAttachment,
    ImageAttachment, ImageDimensions, TextAttachment,
};
use shared_types::{
    ChatAgentConfig, ChatAgentServerConfig, ChatContextServerConfig, ModelEnvBinding,
    ModelEnvBindingSource,
};
use tonic::Status;

pub fn convert_model_provider(grpc_config: GrpcModelProviderConfig) -> ModelProviderConfig {
    ModelProviderConfig {
        id: grpc_config.id,
        name: grpc_config.provider,
        base_url: grpc_config.api_base.unwrap_or_default(),
        api_key: grpc_config.api_key.unwrap_or_default(),
        requires_openai_auth: grpc_config.requires_openai_auth.unwrap_or(true),
        default_model: grpc_config.model,
        api_protocol: grpc_config.api_protocol,
        wire_api: grpc_config.wire_api,
    }
}

pub fn convert_agent_config(grpc_config: GrpcChatAgentConfig) -> Result<ChatAgentConfig, Status> {
    Ok(ChatAgentConfig {
        agent_server: grpc_config
            .agent_server
            .map(convert_agent_server_config)
            .transpose()?,
        context_servers: grpc_config
            .context_servers
            .into_iter()
            .map(|(k, v)| (k, convert_context_server_config(v)))
            .collect(),
        resource_limits: None,
        auto_reload: grpc_config.auto_reload.map(convert_auto_reload_config),
    })
}

pub fn convert_auto_reload_config(grpc_config: GrpcAutoReloadConfig) -> AutoReloadConfig {
    AutoReloadConfig {
        enabled: grpc_config.enabled,
        stability_check_ms: if grpc_config.stability_check_ms == 0 {
            500
        } else {
            grpc_config.stability_check_ms
        },
        stability_retries: if grpc_config.stability_retries == 0 {
            3
        } else {
            grpc_config.stability_retries
        },
        force: grpc_config.force,
    }
}

pub fn convert_agent_server_config(
    grpc_config: GrpcChatAgentServerConfig,
) -> Result<ChatAgentServerConfig, Status> {
    if let Err(err) = shared_types::AgentMode::parse(grpc_config.agent_mode.as_deref()) {
        return Err(Status::invalid_argument(err));
    }

    Ok(ChatAgentServerConfig {
        agent_id: grpc_config.agent_id,
        command: grpc_config.command,
        args: if grpc_config.args.is_empty() {
            None
        } else {
            Some(grpc_config.args)
        },
        env: if grpc_config.env.is_empty() {
            None
        } else {
            Some(grpc_config.env)
        },
        model_env_bindings: grpc_config
            .model_env_bindings
            .into_iter()
            .map(convert_model_env_binding)
            .collect::<Result<Vec<_>, _>>()?,
        agent_mode: grpc_config.agent_mode,
        metadata: if grpc_config.metadata.is_empty() {
            None
        } else {
            Some(grpc_config.metadata)
        },
    })
}

pub fn convert_model_env_binding(grpc_binding: GrpcModelEnvBinding) -> Result<ModelEnvBinding, Status> {
    let source = match GrpcModelEnvBindingSource::try_from(grpc_binding.source) {
        Ok(GrpcModelEnvBindingSource::ApiKey) => ModelEnvBindingSource::ApiKey,
        Ok(GrpcModelEnvBindingSource::BaseUrl) => ModelEnvBindingSource::BaseUrl,
        Ok(GrpcModelEnvBindingSource::DefaultModel) => ModelEnvBindingSource::DefaultModel,
        Ok(GrpcModelEnvBindingSource::ProviderName) => ModelEnvBindingSource::ProviderName,
        Ok(GrpcModelEnvBindingSource::Unspecified) | Err(_) => {
            return Err(Status::invalid_argument(
                "model_env_bindings.source must be specified",
            ));
        }
    };

    if grpc_binding.env_key.trim().is_empty() {
        return Err(Status::invalid_argument(
            "model_env_bindings.env_key must not be empty",
        ));
    }

    Ok(ModelEnvBinding {
        env_key: grpc_binding.env_key,
        source,
    })
}

pub fn convert_context_server_config(
    grpc_config: GrpcChatContextServerConfig,
) -> ChatContextServerConfig {
    ChatContextServerConfig {
        source: grpc_config.source,
        enabled: grpc_config.enabled,
        command: grpc_config.command,
        args: if grpc_config.args.is_empty() {
            None
        } else {
            Some(grpc_config.args)
        },
        env: if grpc_config.env.is_empty() {
            None
        } else {
            Some(grpc_config.env)
        },
    }
}

pub fn convert_attachment_source(
    grpc_source: Option<shared_types::grpc::AttachmentSource>,
) -> Option<AttachmentSource> {
    let source = grpc_source?.source?;
    Some(match source {
        attachment_source::Source::FilePath(path) => AttachmentSource::FilePath { path },
        attachment_source::Source::Base64(data) => AttachmentSource::Base64 {
            data: data.data,
            mime_type: data.mime_type,
        },
        attachment_source::Source::Url(url) => AttachmentSource::Url { url },
    })
}

pub fn convert_attachment(grpc_attachment: shared_types::grpc::Attachment) -> Option<Attachment> {
    let attachment_type = grpc_attachment.attachment_type?;

    Some(match attachment_type {
        attachment::AttachmentType::Text(text) => Attachment::Text(TextAttachment {
            id: text.id,
            source: convert_attachment_source(text.source)?,
            filename: text.filename,
            description: text.description,
        }),
        attachment::AttachmentType::Image(image) => Attachment::Image(ImageAttachment {
            id: image.id,
            source: convert_attachment_source(image.source)?,
            mime_type: image.mime_type,
            filename: image.filename,
            description: image.description,
            dimensions: image.dimensions.map(|d| ImageDimensions {
                width: d.width,
                height: d.height,
            }),
        }),
        attachment::AttachmentType::Audio(audio) => Attachment::Audio(AudioAttachment {
            id: audio.id,
            source: convert_attachment_source(audio.source)?,
            mime_type: audio.mime_type,
            filename: audio.filename,
            description: audio.description,
            duration: audio.duration,
        }),
        attachment::AttachmentType::Document(doc) => Attachment::Document(DocumentAttachment {
            id: doc.id,
            source: convert_attachment_source(doc.source)?,
            mime_type: doc.mime_type,
            filename: doc.filename,
            description: doc.description,
            size: doc.size,
        }),
    })
}

pub fn convert_attachments(grpc_attachments: Vec<shared_types::grpc::Attachment>) -> Vec<Attachment> {
    grpc_attachments
        .into_iter()
        .filter_map(convert_attachment)
        .collect()
}
