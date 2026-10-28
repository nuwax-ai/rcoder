//! 阶段2: 任务下发
//!
//! 对应原流程步骤 5 ~ 8：
//! - 步骤 5: 获取项目工作目录并确保存在
//! - 步骤 6: 构建 ChatPrompt 和 PromptMessage
//! - 步骤 7: 管理 API 密钥配置
//! - 步骤 8: 创建 AgentRequest

use shared_types::{ChatPromptBuilder, error_codes};
use tracing::{error, info};

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
    if !tokio::fs::try_exists(&project_dir).await.unwrap_or(false)
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
        .force_new_session(input.force_new_session)
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

    // 配置只在 launcher 确定实际代理进程并即将 spawn 时登记。
    // 复用、follower、direct 模式及启动前失败均不产生未使用的凭据记录。

    // ========== 步骤8: 直接调用 Agent 会话服务 ==========
    // 创建请求并设置 UUID 和密钥管理器
    Ok(AgentRequest::new(prompt_message, model_provider)
        .with_service_uuid(service_uuid)
        .with_key_manager(Some(context.shared_api_key_manager.clone()))
        .with_project_uuid_map(context.project_uuid_map.clone()))
}
