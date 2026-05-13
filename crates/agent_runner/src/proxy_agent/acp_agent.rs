//! ACP Agent Worker 模块 (SACP 版本)
//!
//! 负责处理 Agent 请求队列，管理 Agent 会话的创建和复用。
//! 使用 AcpSessionManager 进行会话管理。
//!
//! ## SACP 迁移说明
//!
//! - 移除了 `spawn_blocking` + `LocalSet` 模式（SACP 支持 Send trait）
//! - 使用标准 `tokio::spawn` 进行并发处理
//! - 简化了并发模型，提高性能

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dashmap::DashMap;

use agent_abstraction::session::AcpSessionManager;
use anyhow::Result;
use chrono::Utc;
use shared_types::ModelProviderConfig;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use agent_client_protocol::schema::SessionId;

use crate::{
    agent_runtime::get_concurrency_limit,
    model::{AgentStatus, ChatPromptResponse, ProjectAndAgentInfo},
    proxy_agent::SESSION_REQUEST_CONTEXT,
    service::{AGENT_REGISTRY, AgentSessionRegistry, StateAwareNotifier},
    utils::ContentBuilder,
};

/// 🔥 配置标志：是否为无限制模式（HTTP Server 部署）
static IS_UNLIMITED_MODE: AtomicBool = AtomicBool::new(false);

/// 设置运行模式（供 main.rs 调用）
pub fn set_unlimited_mode(enabled: bool) {
    IS_UNLIMITED_MODE.store(enabled, Ordering::SeqCst);
}

// 🔥 OpenTelemetry 追踪
#[cfg(feature = "otel")]
use crate::otel_tracing::RequestSpan;

// 🔥 简化版本：如果没有 OpenTelemetry，使用空的 span
#[cfg(not(feature = "otel"))]
struct RequestSpan;

#[cfg(not(feature = "otel"))]
impl RequestSpan {
    fn new(_project_id: &str, _request_id: &str, _operation: &str) -> Self {
        Self
    }
}

/// Agent 请求结构 (SACP 版本)
///
/// 不再需要 LocalSet，直接在 tokio::spawn 中运行
#[derive(Debug)]
pub struct AgentRequest {
    /// Agent 抽象层的 prompt 消息
    prompt_message: agent_abstraction::PromptMessage,
    /// 发送回执消息的通道
    chat_prompt_tx: oneshot::Sender<ChatPromptResponse>,
    /// 模型提供商配置
    model_provider: Option<ModelProviderConfig>,
    /// 🔥 关联的 service UUID（用于 API 密钥管理）
    service_uuid: Option<String>,
    /// 🔥 共享的 API 密钥管理器（用于自动清理）
    shared_api_key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
    /// 是否跳过槽位限制（HTTP Server 宿主机部署时为 true）
    skip_slot_limit: bool,
}

impl AgentRequest {
    pub fn new(
        prompt_message: agent_abstraction::PromptMessage,
        model_provider: Option<ModelProviderConfig>,
    ) -> (Self, oneshot::Receiver<ChatPromptResponse>) {
        let (chat_prompt_tx, chat_prompt_rx) = oneshot::channel();
        (
            Self {
                prompt_message,
                chat_prompt_tx,
                model_provider,
                service_uuid: None,
                shared_api_key_manager: None,
                skip_slot_limit: false,
            },
            chat_prompt_rx,
        )
    }

    /// 设置 service_uuid
    pub fn with_service_uuid(mut self, service_uuid: Option<String>) -> Self {
        self.service_uuid = service_uuid;
        self
    }

    /// 设置 shared_api_key_manager
    pub fn with_key_manager(
        mut self,
        key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
    ) -> Self {
        self.shared_api_key_manager = key_manager;
        self
    }

    /// 设置是否跳过槽位限制
    pub fn with_skip_slot_limit(mut self, skip: bool) -> Self {
        self.skip_slot_limit = skip;
        self
    }
}

/// Agent Worker 任务
///
/// 使用标准 tokio::spawn 处理 Agent 请求队列。
/// SACP 支持 Send trait，无需 LocalSet。
pub async fn agent_worker(mut request_rx: mpsc::UnboundedReceiver<AgentRequest>) -> Result<()> {
    use agent_abstraction::session::{AcpAgentWorker, AgentWorker, WorkerRequest};

    info!("agent_worker started (SACP version), listening for requests...");

    // 创建 AcpSessionManager，注入 AGENT_REGISTRY 作为 SessionRegistry
    // SACP 版本只需要 2 个泛型参数：N (SessionNotifier) 和 R (SessionRegistry)
    let session_manager = Arc::new(
        AcpSessionManager::<StateAwareNotifier, AgentSessionRegistry>::new(
            Arc::new(StateAwareNotifier::new()),
            AGENT_REGISTRY.clone(),
        ),
    );

    // 创建 AcpAgentWorker
    let worker = AcpAgentWorker::new(session_manager);

    while let Some(request) = request_rx.recv().await {
        let project_id = request.prompt_message.project_id.clone();
        let request_id = request.prompt_message.request_id.clone();

        info!(
            "📨 Received request, project_id: {}, request_id: {}",
            project_id, request_id
        );

        // 1. 预处理附件（agent_runner 特有逻辑）
        let attachment_blocks = if !request.prompt_message.attachments.is_empty() {
            match ContentBuilder::attachments_to_content_blocks(
                &request.prompt_message.attachments,
                &request.prompt_message.project_path,
            )
            .await
            {
                Ok(blocks) => Some(blocks),
                Err(e) => {
                    error!("Attachment processing failed: {:?}", e);

                    if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                        project_id: project_id.clone(),
                        session_id: String::new(),
                        code: shared_types::error_codes::ERR_AGENT_ERROR.to_string(),
                        error: Some(format!(
                            "{}: {:?}",
                            shared_types::error_codes::get_i18n_message_default("error.attachment_processing_failed"),
                            e
                        )),
                        request_id: Some(request_id),
                        service_type: request.prompt_message.service_type.clone(),
                    }) {
                        error!(
                            "Failed to send error response (receiver closed): {:?}",
                            send_err
                        );
                    }
                    continue;
                }
            }
        } else {
            None
        };

        // 2. 创建 WorkerRequest
        let worker_request = WorkerRequest {
            prompt_message: request.prompt_message.clone(),
            model_provider: request.model_provider.clone(),
            attachment_blocks,
            service_uuid: request.service_uuid.clone(),
            shared_api_key_manager: request.shared_api_key_manager.clone(),
        };

        // 3. 调用 AcpAgentWorker 处理（核心业务逻辑）
        let worker_response = match worker.process_request(worker_request).await {
            Ok(response) => response,
            Err(e) => {
                error!("Worker processing failed: {:?}", e);

                if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                    project_id: project_id.clone(),
                    session_id: String::new(),
                    code: shared_types::error_codes::ERR_AGENT_ERROR.to_string(),
                    error: Some(format!(
                        "{}: {:?}",
                        shared_types::error_codes::get_i18n_message_default("error.processing_failed"),
                        e
                    )),
                    request_id: Some(request_id.clone()),
                    service_type: request.prompt_message.service_type.clone(),
                }) {
                    error!(
                        "Failed to send error response (receiver closed): {:?}",
                        send_err
                    );
                }
                continue;
            }
        };

        // 4. 更新全局状态（使用统一的 AGENT_REGISTRY）
        if worker_response.is_new_session {
            if let Some(handles) = &worker_response.session_handles {
                debug!("🆕 New session, registering in AGENT_REGISTRY");

                let project_and_agent_info = ProjectAndAgentInfo {
                    project_id: project_id.clone(),
                    session_id: SessionId::new(Arc::from(worker_response.session_id.as_str())),
                    prompt_tx: handles.prompt_tx.clone(),
                    cancel_tx: handles.cancel_tx.clone(),
                    model_provider: request.model_provider.clone(),
                    request_id: Some(request_id.clone()),
                    status: AgentStatus::Active, // 🆕 修复：Worker 处理中应为 Active，而非 Idle
                    last_activity: Utc::now(),
                    created_at: Utc::now(),
                    stop_handle: handles.lifecycle_handle.clone(),
                };

                // 使用统一的 AGENT_REGISTRY 注册（自动处理所有映射）
                AGENT_REGISTRY.register(
                    &project_id,
                    &worker_response.session_id,
                    project_and_agent_info,
                );

                info!(
                    "🔗 Agent 已注册到 AGENT_REGISTRY: project_id={}, session_id={}",
                    project_id, worker_response.session_id
                );
            }
        } else {
            debug!("♻️ Reusing session, no global Registry update needed");
        }

        // 5. 更新 SESSION_REQUEST_CONTEXT（请求追踪）
        SESSION_REQUEST_CONTEXT.insert(project_id, request_id.clone());

        // 6. 转换并发送回执
        let chat_prompt_response = ChatPromptResponse {
            project_id: worker_response.project_id,
            session_id: worker_response.session_id,
            code: if worker_response.error.is_none() {
                shared_types::error_codes::SUCCESS.to_string()
            } else {
                shared_types::error_codes::ERR_AGENT_ERROR.to_string()
            },
            error: worker_response.error,
            request_id: worker_response.request_id,
            service_type: worker_response.service_type,
        };

        if let Err(e) = request.chat_prompt_tx.send(chat_prompt_response) {
            error!("Failed to send acknowledgment: {:?}", e);
        }
    }

    info!("🛑 agent_worker stopped");
    Ok(())
}

/// 带心跳的 Agent Worker (SACP 版本) - 新架构
///
/// 使用标准 tokio::spawn 进行并发处理（无需 LocalSet）
/// SACP 的 Component<L> trait 要求 Send + 'static，因此可以安全地在多线程环境中使用
///
/// ## 参数变化
///
/// - `request_rx`: 使用有界 channel 代替 unbounded
/// - `state`: 使用 `Arc<AtomicState>` 代替 `WorkerHandle`
/// - `last_heartbeat_ts`: 🔥 P1 修复: 使用 `Arc<AtomicI64>` 代替 `Arc<Mutex<Option<DateTime>>>`
/// - `active_requests`: 直接访问，用于请求追踪
pub async fn agent_worker_with_heartbeat(
    mut request_rx: mpsc::Receiver<AgentRequest>,
    state: Arc<crate::agent_runtime::AtomicState>,
    last_heartbeat_ts: Arc<std::sync::atomic::AtomicI64>,
    _active_requests: Arc<tokio::sync::Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>>,
) -> Result<()> {
    info!("agent_worker started (SACP version with heartbeat), listening for requests...");

    use agent_abstraction::session::{AcpAgentWorker, AgentWorker, WorkerRequest};
    use tokio::time::{Duration, interval};

    // 创建 AcpSessionManager，注入 AGENT_REGISTRY 作为 SessionRegistry
    // SACP 版本只需要 2 个泛型参数：N (SessionNotifier) 和 R (SessionRegistry)
    let session_manager = Arc::new(
        AcpSessionManager::<StateAwareNotifier, AgentSessionRegistry>::new(
            Arc::new(StateAwareNotifier::new()),
            AGENT_REGISTRY.clone(),
        ),
    );

    // 创建 AcpAgentWorker
    let worker = AcpAgentWorker::new(session_manager);

    // 设置状态为 Running（就绪信号）
    state.set(crate::agent_runtime::WorkerState::Running);
    info!("[Worker] SACP Worker initialized, state set to Running");

    // 启动心跳任务 - 🔥 P1 修复: 使用原子操作直接更新 last_heartbeat_ts
    let last_heartbeat_ts_clone = last_heartbeat_ts.clone();
    let heartbeat_task = tokio::spawn(async move {
        let mut heartbeat_interval = interval(Duration::from_secs(5));
        loop {
            heartbeat_interval.tick().await;
            let timestamp = Utc::now();

            // 📊 打印当前 Worker 占用情况
            if IS_UNLIMITED_MODE.load(Ordering::SeqCst) {
                // 无限制模式（HTTP Server 部署）- 不显示具体数量，避免误解
                info!("💓 [Worker] Heartbeat - active sessions: (unlimited)");
            } else {
                // 限制模式（Docker 容器部署）
                let active = AGENT_REGISTRY.stats().agent_count;
                let limit = get_concurrency_limit();
                info!(
                    "💓 [Worker] Heartbeat - active sessions: {}/{}",
                    active, limit
                );
            }

            // 🔥 P1 修复: 使用原子操作直接更新时间戳（无锁）
            last_heartbeat_ts_clone.store(
                timestamp.timestamp_millis(),
                std::sync::atomic::Ordering::Release,
            );
        }
    });

    // 主处理循环 - SACP 版本：使用标准 tokio::spawn 进行并发处理
    while let Some(request) = request_rx.recv().await {
        let project_id = request.prompt_message.project_id.clone();
        let request_id = request.prompt_message.request_id.clone();

        info!(
            "📨 Received request, project_id: {}, request_id: {} - SACP concurrent processing",
            project_id, request_id
        );

        // 克隆需要的变量，用于 spawn 任务
        let worker_clone = worker.clone();

        // 🚀 SACP 版本：直接使用 tokio::spawn（无需 spawn_blocking + LocalSet）
        // SACP 的 Component<L> trait 要求 Send + 'static，因此可以安全地在多线程环境中使用
        tokio::spawn(async move {
            info!(
                "🔵 [SACP] 开始处理请求 project_id={}, request_id={}",
                project_id, request_id
            );

            // 🔥 OpenTelemetry 追踪: 创建请求 span
            let _otel_span = RequestSpan::new(&project_id, &request_id, "process_agent_request");

            // 1. 预处理附件（agent_runner 特有逻辑）
            let attachment_blocks = if !request.prompt_message.attachments.is_empty() {
                match ContentBuilder::attachments_to_content_blocks(
                    &request.prompt_message.attachments,
                    &request.prompt_message.project_path,
                )
                .await
                {
                    Ok(blocks) => Some(blocks),
                    Err(e) => {
                        error!("Attachment processing failed: {:?}", e);

                        // 🔥 DeferGuard 自动清理，无需手动调用 clear_pending_if_exists

                        if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                            project_id: project_id.clone(),
                            session_id: String::new(),
                            code: shared_types::error_codes::ERR_AGENT_ERROR.to_string(),
                            error: Some(format!(
                                "{}: {:?}",
                                shared_types::error_codes::get_i18n_message_default("error.attachment_processing_failed"),
                                e
                            )),
                            request_id: Some(request_id.clone()),
                            service_type: request.prompt_message.service_type.clone(),
                        }) {
                            error!(
                                "Failed to send error response (receiver closed): {:?}",
                                send_err
                            );
                        }
                        return;
                    }
                }
            } else {
                None
            };

            // 2. 创建 WorkerRequest
            let worker_request = WorkerRequest {
                prompt_message: request.prompt_message.clone(),
                model_provider: request.model_provider.clone(),
                attachment_blocks,
                service_uuid: request.service_uuid.clone(),
                shared_api_key_manager: request.shared_api_key_manager.clone(),
            };

            // 3. 调用 AcpAgentWorker 处理（核心业务逻辑）
            let worker_response = match worker_clone.process_request(worker_request).await {
                Ok(response) => response,
                Err(e) => {
                    error!("Worker processing failed: {:?}", e);

                    // 🔥 DeferGuard 自动清理，无需手动调用 clear_pending_if_exists

                    if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                        project_id: project_id.clone(),
                        session_id: String::new(),
                        code: shared_types::error_codes::ERR_AGENT_ERROR.to_string(),
                        error: Some(format!(
                            "{}: {:?}",
                            shared_types::error_codes::get_i18n_message_default("error.processing_failed"),
                            e
                        )),
                        request_id: Some(request_id.clone()),
                        service_type: request.prompt_message.service_type.clone(),
                    }) {
                        error!(
                            "Failed to send error response (receiver closed): {:?}",
                            send_err
                        );
                    }
                    return;
                }
            };

            // 4. 提取 session_handles（在移动 worker_response 之前）
            // 关键：需要保存 lifecycle_handle 用于后续等待会话结束
            let session_handles = worker_response.session_handles.clone();
            let is_new_session = worker_response.is_new_session;
            let response_session_id = worker_response.session_id.clone();

            // 5. 更新全局状态（使用统一的 AGENT_REGISTRY）
            if is_new_session {
                // 🔥 修复：槽位对应 Agent 生命周期，只在创建新 Agent 时获取槽位
                // HTTP Server 部署模式跳过槽位限制
                if request.skip_slot_limit {
                    info!(
                        "⏭️ [原子槽位] 跳过限制（无限制模式）: project_id={}",
                        project_id
                    );
                } else if !AGENT_REGISTRY.try_acquire_session_slot() {
                    let limit = get_concurrency_limit();
                    error!(
                        "🛡️ [原子并发限制] Agent 会话槽位已满 ({}/{}), 拒绝新请求 - project_id={}, request_id={}",
                        AGENT_REGISTRY.active_sessions_count(),
                        limit,
                        project_id,
                        request_id
                    );

                    // 清理 Pending 状态（如果已设置）
                    AGENT_REGISTRY.clear_pending_if_exists(&project_id);

                    if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                        project_id: project_id.clone(),
                        session_id: String::new(),
                        code: shared_types::error_codes::ERR_TOO_MANY_REQUESTS.to_string(),
                        error: Some(shared_types::error_codes::get_i18n_message_default("error.system_busy").to_string().replace("{}", &limit.to_string())),
                        request_id: Some(request_id.clone()),
                        service_type: request.prompt_message.service_type.clone(),
                    }) {
                        error!(
                            "Failed to send reject response (receiver closed): {:?}",
                            send_err
                        );
                    }
                    return;
                } else {
                    info!(
                        "✅ [原子槽位] 成功获取槽位: {}/{} - project_id={}",
                        AGENT_REGISTRY.active_sessions_count(),
                        get_concurrency_limit(),
                        project_id
                    );
                }

                if let Some(ref handles) = session_handles {
                    debug!("🆕 New session, registering in AGENT_REGISTRY");

                    let project_and_agent_info = ProjectAndAgentInfo {
                        project_id: project_id.clone(),
                        session_id: SessionId::new(Arc::from(response_session_id.as_str())),
                        prompt_tx: handles.prompt_tx.clone(),
                        cancel_tx: handles.cancel_tx.clone(),
                        model_provider: request.model_provider.clone(),
                        request_id: Some(request_id.clone()),
                        status: AgentStatus::Active,
                        last_activity: Utc::now(),
                        created_at: Utc::now(),
                        stop_handle: handles.lifecycle_handle.clone(),
                    };

                    AGENT_REGISTRY.register(
                        &project_id,
                        &response_session_id,
                        project_and_agent_info,
                    );

                    info!(
                        "🔗 Agent 已注册到 AGENT_REGISTRY: project_id={}, session_id={}",
                        project_id, response_session_id
                    );
                }
            } else {
                debug!("♻️ Reusing session, no new slot needed (Agent already holds slot)");
            }

            // 6. 更新 SESSION_REQUEST_CONTEXT（请求追踪）
            SESSION_REQUEST_CONTEXT.insert(project_id.clone(), request_id.clone());

            // 7. 转换并发送回执
            let chat_prompt_response = ChatPromptResponse {
                project_id: worker_response.project_id,
                session_id: worker_response.session_id,
                code: if worker_response.error.is_none() {
                    shared_types::error_codes::SUCCESS.to_string()
                } else {
                    shared_types::error_codes::ERR_AGENT_ERROR.to_string()
                },
                error: worker_response.error,
                request_id: worker_response.request_id,
                service_type: worker_response.service_type,
            };

            if let Err(e) = request.chat_prompt_tx.send(chat_prompt_response) {
                error!("Failed to send acknowledgment: {:?}", e);
            } else {
                info!(
                    "✅ 回执已发送，project_id: {}",
                    request.prompt_message.project_id
                );
            }

            // SACP 版本：生命周期管理简化
            //
            // 架构设计：
            // - 槽位对应 Agent 生命周期，而非每次请求
            // - 新会话：等待 Agent 生命周期结束，然后清理
            // - 复用会话：立即退出，不释放槽位（因为 Agent 还在运行）
            if is_new_session {
                if let Some(ref handles) = session_handles {
                    if let Some(ref lifecycle) = handles.lifecycle_handle {
                        info!(
                            "🔄 [SACP] 新会话：等待 Agent 生命周期 - project_id={}, session_id={}",
                            project_id, response_session_id
                        );

                        // 等待以下任一事件：
                        // 1. 用户调用 stop_agent → lifecycle.cancel()
                        // 2. 清理任务停止闲置 Agent（5分钟）→ lifecycle.graceful_stop()
                        // 3. Agent 进程异常退出
                        lifecycle.cancellation_token().cancelled().await;

                        // 🔥 关键修复：lifecycle 结束后，主动清理 Agent 并释放槽位
                        // 使用 session-aware 移除，避免旧 session 的 cleanup 误删新 session 的 registry 条目。
                        // 场景：用户快速发送多条消息时，旧 session 被取消，新 session 注册。
                        // 旧 session 的 spawned task 退出时，registry 中已是新 session 的条目。
                        // 如果用 remove_by_project 会误删新 session，导致新请求超时。

                        AGENT_REGISTRY.remove_by_project_if_session_matches(&project_id, &response_session_id);

                        info!(
                            "🛑 [SACP] Agent 生命周期结束，已清理 Registry - project_id={}, session_id={}",
                            project_id, response_session_id
                        );
                    } else {
                        warn!(
                            "⚠️ [SACP] 新会话缺少 lifecycle_handle - project_id={}",
                            project_id
                        );
                        // 缺少 lifecycle_handle，立即清理
                        AGENT_REGISTRY.remove_by_project_if_session_matches(&project_id, &response_session_id);
                    }
                }
            } else {
                info!(
                    "🔵 [SACP] 复用会话：请求处理完成 - project_id={}, session_id={}",
                    project_id, response_session_id
                );
                // 复用会话时不释放槽位，因为槽位对应 Agent 生命周期
                // 而不是每次请求。Agent 还在运行，槽位应保持占用状态
            }
        });

        // 立即继续循环，接收下一个请求 - 不等待上面的 spawn 完成
    }

    // 清理心跳任务
    heartbeat_task.abort();

    info!("🛑 agent_worker stopped");
    Ok(())
}
