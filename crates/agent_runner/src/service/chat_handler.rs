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
    Attachment, CancelNotificationRequestWrapper, CancelResult, ChatAgentConfig, ChatPromptBuilder,
    ModelProviderConfig, ServiceType, error_codes,
};
use agent_client_protocol::schema::{CancelNotification, SessionId};
use tokio::time::Duration;
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
            error: Some(error_codes::get_i18n_message_default("error.agent_busy")),
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

/// 取消当前正在执行的 Agent 任务
///
/// 发送取消通知并等待取消完成，超时时间为 10 秒
///
/// # Arguments
/// * `cancel_tx` - 取消通知发送通道
/// * `session_id` - 当前会话 ID
/// * `project_id` - 项目 ID
///
/// # Returns
/// * `Ok(())` - 取消成功，Agent 状态已恢复为 Idle
/// * `Err(ChatHandlerOutput)` - 取消失败，包含错误响应
async fn cancel_current_task(
    cancel_tx: &tokio::sync::mpsc::Sender<CancelNotificationRequestWrapper>,
    session_id: &str,
    project_id: &str,
) -> Result<(), ChatHandlerOutput> {
    info!(
        "[ChatHandler] Cancelling current task: project_id={}, session_id={}",
        project_id, session_id
    );

    // 1. 检查 cancel_tx 是否有效
    if cancel_tx.is_closed() {
        error!(
            "[ChatHandler] Cancel channel closed: project_id={}, session_id={}",
            project_id, session_id
        );
        return Err(ChatHandlerOutput::error(
            project_id.to_string(),
            session_id.to_string(),
            error_codes::get_i18n_message_default("error.cancel_channel_closed"),
            error_codes::ERR_SERVICE_UNAVAILABLE.to_string(),
        ));
    }

    // 2. 创建 oneshot channel 等待取消结果
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<CancelResult>();
    let cancel_notification = CancelNotification::new(SessionId::new(Arc::from(session_id)));
    let cancel_request = CancelNotificationRequestWrapper {
        cancel_notification,
        result_tx,
    };

    // 3. 发送取消通知
    if let Err(e) = cancel_tx.send(cancel_request).await {
        error!(
            "[ChatHandler] Failed to send cancel notification: project_id={}, error={}",
            project_id, e
        );
        return Err(ChatHandlerOutput::error(
            project_id.to_string(),
            session_id.to_string(),
            format!(
                "{}: {}",
                error_codes::get_i18n_message_default("error.cancel_failed"),
                e
            ),
            error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
        ));
    }

    // 4. 等待取消结果（超时 10 秒）
    match tokio::time::timeout(Duration::from_secs(10), result_rx).await {
        Ok(Ok(cancel_result)) => {
            if cancel_result.is_success() {
                info!(
                    "[ChatHandler] Cancel notification sent successfully, proceeding with new request: project_id={}, session_id={}",
                    project_id, session_id
                );

                // 🎯 关键设计：cancel 后立即返回，不等待 session 移除
                //
                // 上下文连续性保证：
                // - 不等待 session 移除 → session 保持在 Registry 中
                // - get_or_create_session → is_channel_closed()=false → 复用同一 session
                // - 新 prompt 发送到同一 session 的 prompt_tx → 同一 Agent 子进程处理
                // - Agent 子进程保持存活 → 内存中的对话上下文连续
                //
                // 时序：
                // 1. CancelResult::Success → cancel 通知已发送给 Agent
                // 2. SACP inner loop 收到 cancel → is_cancelled=true → 等待 Agent 响应或超时
                // 3. inner loop 退出 → outer loop 继续等待 prompt_rx
                // 4. 新请求的 prompt 到达 → session_cancelled 重置 → 处理新 prompt
                // 5. 同一 Agent 子进程处理新 prompt → 上下文连续
                //
                // 最坏情况延迟：inner cancel timeout (10s) — Agent 不响应 cancel 时
                Ok(())
            } else {
                let error_msg = cancel_result.error_message().unwrap_or("Unknown error");
                error!(
                    "[ChatHandler] Cancel failed: project_id={}, error={}",
                    project_id, error_msg
                );
                Err(ChatHandlerOutput::error(
                    project_id.to_string(),
                    session_id.to_string(),
                    format!(
                        "{}: {}",
                        error_codes::get_i18n_message_default("error.cancel_failed"),
                        error_msg
                    ),
                    error_codes::ERR_AGENT_ERROR.to_string(),
                ))
            }
        }
        Ok(Err(_)) => {
            error!(
                "[ChatHandler] Cancel result channel dropped: project_id={}",
                project_id
            );
            Err(ChatHandlerOutput::error(
                project_id.to_string(),
                session_id.to_string(),
                error_codes::get_i18n_message_default("error.cancel_channel_dropped"),
                error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
            ))
        }
        Err(_) => {
            error!(
                "[ChatHandler] Cancel timeout (10s): project_id={}",
                project_id
            );
            Err(ChatHandlerOutput::error(
                project_id.to_string(),
                session_id.to_string(),
                error_codes::get_i18n_message_default("error.cancel_timeout"),
                error_codes::ERR_CANCEL_FAILED.to_string(),
            ))
        }
    }
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
        "[ChatHandler] Starting to process request: project_id={}, session_id={:?}, prompt_len={}, has_model_config={}",
        project_id,
        session_id,
        input.prompt.len(),
        input.model_config.is_some()
    );

    // ========== 步骤1: 查询现有 Agent 状态 ==========
    // 优先通过 session_id 查找，回退到 project_id 查找
    let agent_info_ref = if let Some(ref sid) = session_id {
        info!(
            "[ChatHandler] Looking up Agent by session_id: session_id={}",
            sid
        );
        AGENT_REGISTRY.get_agent_info_by_session(sid)
    } else {
        None
    };

    let agent_info_ref = agent_info_ref.or_else(|| {
        info!(
            "[ChatHandler] Looking up Agent by project_id: project_id={}",
            project_id
        );
        AGENT_REGISTRY.get_agent_info(&project_id)
    });

    // ========== 步骤2: 检查 Agent Busy 状态，如果忙则取消当前任务 ==========
    use crate::model::AgentStatus;
    if let Some(agent_info) = agent_info_ref {
        if agent_info.status == AgentStatus::Active || agent_info.status == AgentStatus::Pending {
            info!(
                "[ChatHandler] Agent Busy, cancelling current task: project_id={}, status={:?}, session_id={:?}",
                project_id, agent_info.status, session_id
            );

            // 获取 cancel_tx 和 session_id，并释放 DashMap 读锁（防死锁）
            let cancel_tx = agent_info.cancel_tx.clone();
            // 优先使用请求中的 session_id，如果为空则从 agent_info 中获取
            let actual_session_id = session_id
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| agent_info.session_id.to_string());
            drop(agent_info);

            // 取消当前任务
            if let Err(cancel_error) =
                cancel_current_task(&cancel_tx, &actual_session_id, &project_id).await
            {
                // 取消失败，返回错误
                error!(
                    "[ChatHandler] Failed to cancel current task: project_id={}, error={:?}",
                    project_id, cancel_error
                );
                return cancel_error;
            }

            info!(
                "[ChatHandler] Current task cancelled, proceeding with new request: project_id={}",
                project_id
            );
        }
    }

    // ========== 步骤3: 创建 PendingGuard（RAII）==========
    // 自动在作用域结束时清理，避免状态泄漏
    let pending_guard = PendingGuard::new(&AGENT_REGISTRY, &project_id);
    info!(
        "[ChatHandler] Created PendingGuard: project_id={}",
        project_id
    );

    // ========== 步骤4: 清理无效 session ==========
    // 只在 session 不存在时才清理无效的 session_id
    if let Some(ref sid) = session_id {
        let session_exists = AGENT_REGISTRY.contains_session(sid);

        if !session_exists && SESSION_CACHE.remove(sid).is_some() {
            info!(
                "[ChatHandler] session does not exist, removing invalid session: session_id={}",
                sid
            );
        } else if session_exists {
            info!("[ChatHandler] Reusing existing session: session_id={}", sid);
        }
    }

    // ========== 步骤5: 获取项目工作目录 ==========
    let project_dir = input.project_dir.clone();
    info!(
        "[ChatHandler] Project working directory: {:?}, service_type={:?}",
        project_dir, input.service_type
    );

    // 确保目录存在
    if !project_dir.exists() {
        if let Err(e) = tokio::fs::create_dir_all(&project_dir).await {
            error!("[ChatHandler] Failed to create project directory: {}", e);
            return ChatHandlerOutput::error(
                project_id,
                session_id.unwrap_or_default(),
                format!(
                    "{}: {}",
                    error_codes::get_i18n_message_default("error.create_project_dir_failed"),
                    e
                ),
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
            error!("[ChatHandler] Failed to build ChatPrompt: {}", e);
            return ChatHandlerOutput::error(
                project_id,
                session_id.unwrap_or_default(),
                format!(
                    "{}: {}",
                    error_codes::get_i18n_message_default("error.build_chat_prompt_failed"),
                    e
                ),
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
            "[ChatHandler] Stored API config: service_uuid={}, provider_name={}, base_url={}",
            service_uuid_ref,
            provider.name,
            shared_types::mask_url(&provider.base_url)
        );
    } else {
        warn!("[ChatHandler] No model config provided; falling back to env vars or defaults");
    }

    // ========== 步骤8: 检查 Worker 状态 ==========
    match context.agent_runtime.state() {
        WorkerState::Running => {
            // 正常操作，继续处理
        }
        WorkerState::Starting => {
            warn!("[ChatHandler] Agent Worker is starting; request may be delayed");
        }
        WorkerState::Stopping | WorkerState::Stopped => {
            // PendingGuard 自动清理（在 drop 时）
            error!("[ChatHandler] Agent Worker unavailable");
            return ChatHandlerOutput::error(
                project_id,
                session_id.unwrap_or_default(),
                error_codes::get_i18n_message_default("error.agent_worker_unavailable"),
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
        error!("[ChatHandler] Failed to send task: {}", e);
        return ChatHandlerOutput::error(
            project_id,
            session_id.unwrap_or_default(),
            format!(
                "{}: {}",
                error_codes::get_i18n_message_default("error.send_task_failed"),
                e
            ),
            error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
        );
    }

    // ========== 步骤10: 等待响应（5 分钟超时）==========
    match tokio::time::timeout(std::time::Duration::from_secs(300), chat_prompt_rx).await {
        Ok(Ok(response)) => {
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
                "[ChatHandler] Chat completed: success={}, session_id={}",
                output.success, output.session_id
            );

            // 只有请求成功时才提交 PendingGuard 保留 Pending 状态
            // 失败时 PendingGuard 自动 drop 清理，允许下次请求重新创建 Agent
            if output.success {
                pending_guard.commit_success();
            }

            output
        }
        Ok(Err(e)) => {
            // PendingGuard 自动清理（在 drop 时）
            error!("[ChatHandler] Chat response channel dropped: {}", e);
            ChatHandlerOutput::error(
                project_id,
                session_id.unwrap_or_default(),
                format!(
                    "{}: {}",
                    error_codes::get_i18n_message_default("error.request_processing_failed"),
                    e
                ),
                error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
            )
        }
        Err(_elapsed) => {
            // PendingGuard 自动清理（在 drop 时）
            error!("[ChatHandler] ⏰ Chat request timeout (300s): project_id={}", project_id);
            ChatHandlerOutput::error(
                project_id,
                session_id.unwrap_or_default(),
                error_codes::get_i18n_message_default("error.request_processing_failed")
                    + ": request timeout (300s)",
                error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
            )
        }
    }
}
