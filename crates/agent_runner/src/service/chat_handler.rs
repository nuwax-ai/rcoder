//! Chat Handler 共享逻辑
//!
//! 封装 chat 请求处理的核心业务逻辑，供 gRPC 和 HTTP 复用。
//!
//! ## 设计原则
//!
//! - **RAII 状态管理**: 使用 PendingGuard 自动管理 Pending 状态
//! - **DashMap Entry API**: 避免读写锁竞态条件
//! - **Fail Fast**: 尽早暴露问题，便于定位修复

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use shared_types::{
    Attachment, ChatAgentConfig, ChatPromptBuilder, ModelProviderConfig, ServiceType, error_codes,
};
use tracing::{debug, error, info, warn};

use crate::AgentRuntime;
use crate::agent_runtime::WorkerState;
use crate::proxy_agent::AgentRequest;
use crate::service::{AGENT_REGISTRY, PendingGuard, SESSION_CACHE};

/// Chat Handler 输入参数
///
/// 包含处理 chat 请求所需的所有参数，与协议无关（gRPC/HTTP）
#[derive(Debug, Clone)]
pub struct ChatHandlerInput {
    /// 项目 ID
    pub project_id: String,
    /// 项目工作目录（由调用方根据环境决定）
    pub project_dir: PathBuf,
    /// 会话 ID（可选，用于复用会话）
    pub session_id: Option<String>,
    /// 用户提示词
    pub prompt: String,
    /// 请求 ID（用于追踪）
    pub request_id: String,
    /// 附件列表
    pub attachments: Vec<Attachment>,
    /// 数据源附件列表
    pub data_source_attachments: Vec<String>,
    /// 模型配置（可选）
    pub model_config: Option<ModelProviderConfig>,
    /// 服务类型
    pub service_type: ServiceType,
    /// Agent 配置覆盖（可选）
    pub agent_config_override: Option<ChatAgentConfig>,
    /// 系统提示覆盖（可选）
    pub system_prompt_override: Option<String>,
    /// 用户提示模板覆盖（可选）
    pub user_prompt_template_override: Option<String>,
    /// 是否跳过槽位限制（HTTP Server 宿主机部署时为 true）
    pub skip_slot_limit: bool,
}

/// Chat Handler 输出结果
///
/// 统一的响应结构，可转换为 gRPC 或 HTTP 响应
#[derive(Debug, Clone)]
pub struct ChatHandlerOutput {
    /// 项目 ID
    pub project_id: String,
    /// 会话 ID
    pub session_id: String,
    /// 是否成功
    pub success: bool,
    /// 错误消息（可选）
    pub error: Option<String>,
    /// 错误码（可选）
    pub error_code: Option<String>,
    /// 请求 ID（可选）
    pub request_id: Option<String>,
    /// 是否需要降级处理
    pub need_fallback: bool,
    /// 降级原因（可选）
    pub fallback_reason: Option<String>,
}

impl ChatHandlerOutput {
    /// 创建错误响应
    pub fn error(
        project_id: String,
        session_id: String,
        error_msg: String,
        error_code: String,
    ) -> Self {
        Self {
            project_id,
            session_id,
            success: false,
            error: Some(error_msg),
            error_code: Some(error_code),
            request_id: None,
            need_fallback: false,
            fallback_reason: None,
        }
    }

    /// 创建 Agent Busy 错误响应
    pub fn agent_busy(project_id: String, session_id: Option<String>) -> Self {
        Self {
            project_id,
            session_id: session_id.unwrap_or_default(),
            success: false,
            error: Some("Agent 正在执行任务，请等待当前任务完成后再发送新请求".to_string()),
            error_code: Some(error_codes::ERR_AGENT_BUSY.to_string()),
            request_id: None,
            need_fallback: false,
            fallback_reason: None,
        }
    }
}

/// Chat Handler 依赖上下文
///
/// 包含处理 chat 请求所需的运行时依赖
pub struct ChatHandlerContext {
    /// Agent 运行时
    pub agent_runtime: Arc<AgentRuntime>,
    /// 共享的 API 密钥管理器
    pub shared_api_key_manager: Arc<DashMap<String, ModelProviderConfig>>,
    /// project_id -> UUID 映射
    pub project_uuid_map: Arc<DashMap<String, String>>,
}

/// 执行 Chat 请求的核心逻辑
///
/// 封装了 chat 请求的完整处理流程：
/// 1. Agent Busy 检查
/// 2. PendingGuard RAII 状态管理
/// 3. Session 清理逻辑
/// 4. 目录创建
/// 5. ChatPrompt 构建
/// 6. API Key 管理
/// 7. AgentRequest 创建和发送
/// 8. 等待响应
///
/// # Arguments
///
/// * `input` - Chat 请求输入参数
/// * `context` - 运行时上下文依赖
///
/// # Returns
///
/// 返回统一的 `ChatHandlerOutput` 结果
pub async fn handle_chat_core(
    input: ChatHandlerInput,
    context: &ChatHandlerContext,
) -> ChatHandlerOutput {
    let project_id = input.project_id.clone();
    let session_id = input.session_id.clone();
    let request_id = input.request_id.clone();

    info!(
        "[ChatHandler] 开始处理请求: project_id={}, session_id={:?}, prompt_len={}, has_model_config={}",
        project_id,
        session_id,
        input.prompt.len(),
        input.model_config.is_some()
    );

    // ========== 步骤1: 查询现有 Agent 状态 ==========
    // 优先通过 session_id 查找，回退到 project_id 查找
    let agent_info_ref = if let Some(ref sid) = session_id {
        info!(
            "[ChatHandler] 通过 session_id 查找 Agent: session_id={}",
            sid
        );
        AGENT_REGISTRY.get_agent_info_by_session(sid)
    } else {
        None
    };

    let agent_info_ref = agent_info_ref.or_else(|| {
        info!(
            "[ChatHandler] 通过 project_id 查找 Agent: project_id={}",
            project_id
        );
        AGENT_REGISTRY.get_agent_info(&project_id)
    });

    // ========== 步骤2: 检查 Agent Busy 状态 ==========
    use crate::model::AgentStatus;
    if let Some(agent_info) = agent_info_ref {
        if agent_info.status == AgentStatus::Active || agent_info.status == AgentStatus::Pending {
            info!(
                "[ChatHandler] Agent Busy，返回 9010 错误: project_id={}, status={:?}, session_id={:?}",
                project_id, agent_info.status, session_id
            );
            return ChatHandlerOutput::agent_busy(project_id, session_id);
        }
    }

    // ========== 步骤3: 创建 PendingGuard（RAII）==========
    // 自动在作用域结束时清理，避免状态泄漏
    let pending_guard = PendingGuard::new(&AGENT_REGISTRY, &project_id);
    info!("[ChatHandler] 创建 PendingGuard: project_id={}", project_id);

    // ========== 步骤4: 清理无效 session ==========
    // 只在 session 不存在时才清理无效的 session_id
    if let Some(ref sid) = session_id {
        let session_exists = AGENT_REGISTRY.contains_session(sid);

        if !session_exists && SESSION_CACHE.remove(sid).is_some() {
            info!(
                "[ChatHandler] session 不存在，移除无效 session: session_id={}",
                sid
            );
        } else if session_exists {
            info!("[ChatHandler] 复用已存在的 session: session_id={}", sid);
        }
    }

    // ========== 步骤5: 获取项目工作目录 ==========
    let project_dir = input.project_dir.clone();
    info!(
        "[ChatHandler] 项目工作目录: {:?}, service_type={:?}",
        project_dir, input.service_type
    );

    // 确保目录存在
    if !project_dir.exists() {
        if let Err(e) = tokio::fs::create_dir_all(&project_dir).await {
            error!("[ChatHandler] 创建项目目录失败: {}", e);
            return ChatHandlerOutput::error(
                project_id,
                session_id.unwrap_or_default(),
                format!("创建项目目录失败: {}", e),
                error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
            );
        }
    }

    // ========== 步骤6: 构建 ChatPrompt 和 PromptMessage ==========
    let chat_prompt = match ChatPromptBuilder::default()
        .project_id(project_id.clone())
        .project_path(project_dir)
        .session_id(session_id.clone())
        .prompt(input.prompt)
        .attachments(input.attachments)
        .data_source_attachments(input.data_source_attachments)
        .service_type(input.service_type)
        .request_id(request_id.clone())
        .system_prompt_override(input.system_prompt_override)
        .user_prompt_template_override(input.user_prompt_template_override)
        .agent_config_override(input.agent_config_override)
        .build()
    {
        Ok(prompt) => prompt,
        Err(e) => {
            error!("[ChatHandler] 构建 ChatPrompt 失败: {}", e);
            return ChatHandlerOutput::error(
                project_id,
                session_id.unwrap_or_default(),
                format!("构建 ChatPrompt 失败: {}", e),
                error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
            );
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
    if let (Some(ref provider), Some(ref service_uuid_ref)) =
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
            .insert(service_uuid_ref.to_string(), (*provider).clone());

        // 存储 project_id -> UUID 映射（用于后续清理时查找）
        context
            .project_uuid_map
            .insert(project_id.clone(), service_uuid_ref.to_string());

        info!(
            "[ChatHandler] 已存储 API 配置: service_uuid={}, provider_name={}, base_url={}",
            service_uuid_ref,
            provider.name,
            shared_types::mask_url(&provider.base_url)
        );
    } else {
        warn!("[ChatHandler] 未提供模型配置，将使用环境变量或默认配置");
    }

    // ========== 步骤8: 检查 Worker 状态 ==========
    match context.agent_runtime.state() {
        WorkerState::Running => {
            // 正常操作，继续处理
        }
        WorkerState::Starting => {
            warn!("[ChatHandler] Agent Worker 正在启动，请求可能会延迟");
        }
        WorkerState::Stopping | WorkerState::Stopped => {
            // PendingGuard 自动清理（在 drop 时）
            error!("[ChatHandler] Agent Worker 不可用");
            return ChatHandlerOutput::error(
                project_id,
                session_id.unwrap_or_default(),
                "Agent Worker 不可用，正在重启，请稍后重试".to_string(),
                error_codes::ERR_SERVICE_UNAVAILABLE.to_string(),
            );
        }
    }

    // ========== 步骤9: 发送任务到 Agent Worker ==========
    // 创建请求并设置 UUID 和密钥管理器
    let (agent_request, chat_prompt_rx) = AgentRequest::new(prompt_message, model_provider);
    let agent_request = agent_request
        .with_service_uuid(service_uuid)
        .with_key_manager(Some(context.shared_api_key_manager.clone()))
        .with_skip_slot_limit(input.skip_slot_limit);

    if let Err(e) = context.agent_runtime.send(agent_request).await {
        // PendingGuard 自动清理（在 drop 时）
        error!("[ChatHandler] 发送任务失败: {}", e);
        return ChatHandlerOutput::error(
            project_id,
            session_id.unwrap_or_default(),
            format!("发送任务失败: {}", e),
            error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
        );
    }

    // ========== 步骤10: 等待响应 ==========
    match chat_prompt_rx.await {
        Ok(response) => {
            let output = ChatHandlerOutput {
                project_id: response.project_id,
                session_id: response.session_id,
                success: response.error.is_none(),
                error: response.error,
                error_code: if response.code != error_codes::SUCCESS {
                    Some(response.code)
                } else {
                    None
                },
                request_id: Some(request_id),
                need_fallback: false,
                fallback_reason: None,
            };

            info!(
                "[ChatHandler] Chat 完成: success={}, session_id={}",
                output.success, output.session_id
            );

            // 请求成功，提交 PendingGuard 保留 Pending 状态
            // Agent 已成功启动，Pending 状态将由后续操作转换为 Active
            pending_guard.commit_success();

            output
        }
        Err(e) => {
            // PendingGuard 自动清理（在 drop 时）
            error!("[ChatHandler] Chat 失败: {}", e);
            ChatHandlerOutput::error(
                project_id,
                session_id.unwrap_or_default(),
                format!("处理请求失败: {}", e),
                error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
            )
        }
    }
}
