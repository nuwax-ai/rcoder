//! 阶段2: 任务下发
//!
//! 对应原流程步骤 5 ~ 8：
//! - 步骤 5: 获取项目工作目录并确保存在
//! - 步骤 6: 构建 ChatPrompt 和 PromptMessage
//! - 步骤 7: 管理 API 密钥配置
//! - 步骤 8: 创建 AgentRequest

use shared_types::{ChatPromptBuilder, error_codes};
use tracing::{debug, error, info, warn};

use super::types::{ChatHandlerContext, ChatHandlerInput, ChatHandlerOutput, SessionPreparation};
use crate::service::AgentRequest;

/// 构建 ChatPrompt、管理 API 密钥、创建 AgentRequest。
pub(super) async fn dispatch_task(
    input: ChatHandlerInput,
    context: &ChatHandlerContext,
    project_id: &str,
    session_id: &Option<String>,
    request_id: &str,
    preparation: &SessionPreparation,
) -> Result<AgentRequest, Box<ChatHandlerOutput>> {
    // ========== 步骤5: 获取项目工作目录 ==========
    let project_dir = input.project_dir.clone();
    info!(
        "[ChatHandler] Project working directory: {:?}, service_type={:?}",
        project_dir, input.service_type
    );

    // 确保目录存在
    if !project_dir.exists()
        && let Err(e) = tokio::fs::create_dir_all(&project_dir).await
    {
        error!("[ChatHandler] Failed to create project directory: {}", e);
        return Err(Box::new(ChatHandlerOutput::error(
            project_id.to_string(),
            session_id.clone().unwrap_or_default(),
            format!(
                "{}: {}",
                error_codes::get_i18n_message_default("error.create_project_dir_failed"),
                e
            ),
            error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
        )));
    }

    // ========== 步骤6: 构建 ChatPrompt 和 PromptMessage ==========

    // 如果是 auto_reload 重启，使用旧 session_id 作为 resume_session_id
    let session_id_for_prompt = if preparation.was_reloaded {
        preparation.resume_session_id.clone()
    } else {
        session_id.clone()
    };

    let chat_prompt = match ChatPromptBuilder::default()
        .project_id(project_id.to_string())
        .project_path(project_dir)
        .session_id(session_id_for_prompt)
        .prompt(input.prompt)
        .attachments(input.attachments)
        .data_source_attachments(input.data_source_attachments)
        .service_type(input.service_type)
        .user_id(input.user_id)
        .request_id(request_id.to_string())
        .model_provider(input.model_config.clone())
        .system_prompt_override(input.system_prompt_override)
        .user_prompt_template_override(input.user_prompt_template_override)
        .agent_config_override(input.agent_config_override)
        .is_devcomputer(input.is_devcomputer)
        .build()
    {
        Ok(prompt) => prompt,
        Err(e) => {
            error!("[ChatHandler] Failed to build ChatPrompt: {}", e);
            return Err(Box::new(ChatHandlerOutput::error(
                project_id.to_string(),
                session_id.clone().unwrap_or_default(),
                format!(
                    "{}: {}",
                    error_codes::get_i18n_message_default("error.build_chat_prompt_failed"),
                    e
                ),
                error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
            )));
        }
    };

    // 转换为 PromptMessage
    let prompt_message = agent_abstraction::PromptMessage::from(chat_prompt);

    // ========== 步骤7: 管理 API 密钥配置 ==========
    let model_provider = input.model_config;

    // 生成唯一的 service UUID（用于 API 密钥管理）
    let service_uuid = if model_provider.is_some() {
        Some(uuid::Uuid::new_v4().to_string())
    } else {
        None
    };

    // 存储 API 配置到共享 DashMap
    if let (Some(provider), Some(ref service_uuid_ref)) =
        (model_provider.as_ref(), service_uuid.as_ref())
    {
        debug!(
            "[ChatHandler] 使用模型配置: provider={}, model={}, base_url={}, api_protocol={:?}, requires_openai_auth={}, service_uuid={}",
            provider.name,
            provider.default_model,
            provider.base_url,
            provider.api_protocol,
            provider.requires_openai_auth,
            service_uuid_ref
        );

        // 存储 ModelProviderConfig 到共享 DashMap（使用 UUID 作为 key）
        context
            .shared_api_key_manager
            .insert(service_uuid_ref.to_string(), provider.clone());

        // 存储 project_id -> UUID 映射（用于后续清理时查找）。多轮 chat 每轮
        // 生成新 uuid 覆盖旧映射——insert 拿回旧 uuid（DashMap 返回旧 value）
        // 并连带清理其 api_key 条目，否则旧 uuid 的 ModelProviderConfig（含
        // 明文 api_key）成为永不清理的孤儿（stop 路径只清"当前映射指向的 uuid"）
        if let Some(old_uuid) = context
            .project_uuid_map
            .insert(project_id.to_string(), service_uuid_ref.to_string())
        {
            context.shared_api_key_manager.remove(&old_uuid);
            debug!("[ChatHandler] Replaced API config mapping, evicted old uuid={old_uuid}");
        }

        info!(
            "[ChatHandler] Stored API config: service_uuid={}, provider_name={}, base_url={}",
            service_uuid_ref,
            provider.name,
            shared_types::mask_url(&provider.base_url)
        );
    } else {
        warn!("[ChatHandler] No model config provided; falling back to env vars or defaults");
    }

    // ========== 步骤8: 直接调用 Agent 会话服务 ==========
    // 创建请求并设置 UUID 和密钥管理器
    Ok(AgentRequest::new(prompt_message, model_provider)
        .with_service_uuid(service_uuid)
        .with_key_manager(Some(context.shared_api_key_manager.clone())))
}
