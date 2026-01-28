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

use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;

use agent_abstraction::session::AcpSessionManager;
use anyhow::Result;
use chrono::Utc;
use shared_types::ModelProviderConfig;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

// SACP 类型导入
use sacp::schema::SessionId;

use crate::{
    agent_runtime::get_concurrency_limit,
    grpc::agent_service_impl::predict_needs_new_session_sync,
    model::{AgentStatus, ChatPromptResponse, ProjectAndAgentInfo},
    proxy_agent::SESSION_REQUEST_CONTEXT,
    service::{AGENT_REGISTRY, AgentSessionRegistry, StateAwareNotifier},
    utils::ContentBuilder,
};

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
}

// 向后兼容类型别名（SACP 迁移）
/// 旧名称别名，保持向后兼容
#[deprecated(since = "0.1.0", note = "请使用 AgentRequest 代替")]
pub type LocalSetAgentRequest = AgentRequest;

/// Agent Worker 任务 (SACP 版本)
///
/// 使用标准 tokio::spawn 处理 Agent 请求队列。
/// SACP 支持 Send trait，无需 LocalSet。
pub async fn agent_worker(
    mut request_rx: mpsc::UnboundedReceiver<AgentRequest>,
) -> Result<()> {
    use agent_abstraction::session::{AcpAgentWorker, AgentWorker, WorkerRequest};

    info!("🚀 agent_worker 启动（SACP 版本），开始监听请求...");

    // 创建 AcpSessionManager，注入 AGENT_REGISTRY 作为 SessionRegistry
    // SACP 版本只需要 2 个泛型参数：N (SessionNotifier) 和 R (SessionRegistry)
    let session_manager = Arc::new(AcpSessionManager::<
        StateAwareNotifier,
        AgentSessionRegistry,
    >::new(
        Arc::new(StateAwareNotifier::new()),
        AGENT_REGISTRY.clone(),
    ));

    // 创建 AcpAgentWorker
    let worker = AcpAgentWorker::new(session_manager);

    while let Some(request) = request_rx.recv().await {
        let project_id = request.prompt_message.project_id.clone();
        let request_id = request.prompt_message.request_id.clone();

        info!(
            "📨 接收到请求，project_id: {}, request_id: {}",
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
                    error!("❌ 附件处理失败: {:?}", e);

                    if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                        project_id: project_id.clone(),
                        session_id: String::new(),
                        code: shared_types::error_codes::ERR_AGENT_ERROR.to_string(),
                        error: Some(format!("附件处理失败: {:?}", e)),
                        request_id: Some(request_id),
                        service_type: request.prompt_message.service_type.clone(),
                    }) {
                        error!("❌ 发送错误响应失败（接收端已关闭）: {:?}", send_err);
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
                error!("❌ Worker 处理失败: {:?}", e);

                if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                    project_id: project_id.clone(),
                    session_id: String::new(),
                    code: shared_types::error_codes::ERR_AGENT_ERROR.to_string(),
                    error: Some(format!("处理失败: {:?}", e)),
                    request_id: Some(request_id.clone()),
                    service_type: request.prompt_message.service_type.clone(),
                }) {
                    error!("❌ 发送错误响应失败（接收端已关闭）: {:?}", send_err);
                }
                continue;
            }
        };

        // 4. 更新全局状态（使用统一的 AGENT_REGISTRY）
        if worker_response.is_new_session {
            if let Some(handles) = &worker_response.session_handles {
                debug!("🆕 新会话，注册到 AGENT_REGISTRY");

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
            debug!("♻️ 复用会话，无需更新全局 Registry");
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
            error!("❌ 发送回执失败: {:?}", e);
        }
    }

    info!("🛑 agent_worker 停止");
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
    active_requests: Arc<tokio::sync::Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>>,
) -> Result<()> {
    info!("🚀 agent_worker 启动（SACP 版本，带心跳支持），开始监听请求...");

    use agent_abstraction::session::{AcpAgentWorker, AgentWorker, WorkerRequest};
    use tokio::time::{Duration, interval};

    // 创建 AcpSessionManager，注入 AGENT_REGISTRY 作为 SessionRegistry
    // SACP 版本只需要 2 个泛型参数：N (SessionNotifier) 和 R (SessionRegistry)
    let session_manager = Arc::new(AcpSessionManager::<
        StateAwareNotifier,
        AgentSessionRegistry,
    >::new(
        Arc::new(StateAwareNotifier::new()),
        AGENT_REGISTRY.clone(),
    ));

    // 创建 AcpAgentWorker
    let worker = AcpAgentWorker::new(session_manager);

    // 设置状态为 Running（就绪信号）
    state.set(crate::agent_runtime::WorkerState::Running);
    info!("✅ [Worker] SACP Worker 初始化完成，状态设置为 Running");

    // 启动心跳任务 - 🔥 P1 修复: 使用原子操作直接更新 last_heartbeat_ts
    let last_heartbeat_ts_clone = last_heartbeat_ts.clone();
    let heartbeat_task = tokio::spawn(async move {
        let mut heartbeat_interval = interval(Duration::from_secs(5));
        loop {
            heartbeat_interval.tick().await;

            // 📊 获取当前活跃 Worker 数量
            let active_count = AGENT_REGISTRY.stats().agent_count;
            let total_count = get_concurrency_limit();

            let timestamp = Utc::now();

            // 📊 打印当前 Worker 占用情况
            info!(
                "💓 [Worker] 心跳 - 当前活跃: {}/{}, 可用: {}, 时间: {}",
                active_count,
                total_count,
                total_count.saturating_sub(active_count),
                timestamp.format("%Y-%m-%d %H:%M:%S")
            );

            // 🔥 P1 修复: 使用原子操作直接更新时间戳（无锁）
            last_heartbeat_ts_clone.store(timestamp.timestamp_millis(), std::sync::atomic::Ordering::Release);
        }
    });

    // 主处理循环 - SACP 版本：使用标准 tokio::spawn 进行并发处理
    while let Some(request) = request_rx.recv().await {
        let project_id = request.prompt_message.project_id.clone();
        let request_id = request.prompt_message.request_id.clone();

        info!(
            "📨 接收到请求，project_id: {}, request_id: {} - SACP 并发处理",
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
            //
            // 🆕 注意：并发槽位检查已迁移到 gRPC 层（最早拒绝点优化）
            // - gRPC 层在创建 PendingGuard 后立即检查槽位
            // - 如果槽位已满，直接拒绝，不消耗 Worker 资源
            // - 这里只需要处理请求，无需重复检查槽位
            //
            // ⚠️ 重要：gRPC 层已经预留了槽位（如果需要新会话）
            // - 如果处理失败，gRPC 层的 PendingGuard 会自动清理
            // - 如果 gRPC 预测错误（极少见），Worker 层会处理补偿逻辑
            let attachment_blocks = if !request.prompt_message.attachments.is_empty() {
                match ContentBuilder::attachments_to_content_blocks(
                    &request.prompt_message.attachments,
                    &request.prompt_message.project_path,
                )
                .await
                {
                    Ok(blocks) => Some(blocks),
                    Err(e) => {
                        error!("❌ 附件处理失败: {:?}", e);

                        // 🔥 DeferGuard 自动清理，无需手动调用 clear_pending_if_exists
                        // 注意：gRPC 层的 PendingGuard 会自动清理 Pending 状态
                        // 如果 gRPC 层预留了槽位，也会在 PendingGuard Drop 时释放

                        if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                            project_id: project_id.clone(),
                            session_id: String::new(),
                            code: shared_types::error_codes::ERR_AGENT_ERROR.to_string(),
                            error: Some(format!("附件处理失败: {:?}", e)),
                            request_id: Some(request_id.clone()),
                            service_type: request.prompt_message.service_type.clone(),
                        }) {
                            error!("❌ 发送错误响应失败（接收端已关闭）: {:?}", send_err);
                        }
                        return;
                    }
                }
            } else {
                None
            };

            // 3. 创建 WorkerRequest
            let worker_request = WorkerRequest {
                prompt_message: request.prompt_message.clone(),
                model_provider: request.model_provider.clone(),
                attachment_blocks,
                service_uuid: request.service_uuid.clone(),
                shared_api_key_manager: request.shared_api_key_manager.clone(),
            };

            // 4. 调用 AcpAgentWorker 处理（核心业务逻辑）
            let worker_response = match worker_clone.process_request(worker_request).await {
                Ok(response) => response,
                Err(e) => {
                    error!("❌ Worker 处理失败: {:?}", e);

                    // 🔥 DeferGuard 自动清理，无需手动调用 clear_pending_if_exists
                    // 注意：gRPC 层的 PendingGuard 会自动清理 Pending 状态
                    // 如果 gRPC 层预留了槽位，也会在 PendingGuard Drop 时释放

                    if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                        project_id: project_id.clone(),
                        session_id: String::new(),
                        code: shared_types::error_codes::ERR_AGENT_ERROR.to_string(),
                        error: Some(format!("处理失败: {:?}", e)),
                        request_id: Some(request_id.clone()),
                        service_type: request.prompt_message.service_type.clone(),
                    }) {
                        error!("❌ 发送错误响应失败（接收端已关闭）: {:?}", send_err);
                    }
                    return;
                }
            };

            // 5. 提取 session_handles（在移动 worker_response 之前）
            // 关键：需要保存 lifecycle_handle 用于后续等待会话结束
            let session_handles = worker_response.session_handles.clone();
            let is_new_session = worker_response.is_new_session;
            let response_session_id = worker_response.session_id.clone();

            // 6. 验证预测并更新全局状态
            //
            // 🆕 注意：gRPC 层已经进行了并发槽位检查
            // - 如果 gRPC 预测需要新会话，已经预留了槽位
            // - 如果 gRPC 预测复用现有会话，没有预留槽位
            //
            // 这里只需要处理极少数的预测错误情况：
            // - gRPC 预测复用但实际新会话：需要获取槽位
            // - gRPC 预测新会话但实际复用：需要释放槽位（预留错误）
            if is_new_session {
                // 新会话：检查 gRPC 层是否预留了槽位
                // gRPC 层通过 predict_needs_new_session_sync() 预测
                let grpc_predicted_new_session = predict_needs_new_session_sync(
                    &project_id,
                    &request.prompt_message.session_id,
                );

                if grpc_predicted_new_session {
                    // gRPC 预测正确，已预留槽位，直接注册
                    info!(
                        "✅ [原子槽位] gRPC 预测正确，使用预留槽位 - project_id={}",
                        project_id
                    );
                } else {
                    // 预测错误：gRPC 预测复用但实际创建了新会话，需要获取槽位
                    warn!(
                        "⚠️ [原子槽位] 预测错误（gRPC预测复用但实际新会话），尝试获取槽位 - project_id={}",
                        project_id
                    );
                    if !AGENT_REGISTRY.try_acquire_session_slot() {
                        let limit = get_concurrency_limit();
                        error!(
                            "🛡️ [原子并发限制] Agent 会话槽位已满 ({}/{}), 无法处理新会话 - project_id={}",
                            AGENT_REGISTRY.active_sessions_count(),
                            limit,
                            project_id
                        );
                        // 清理 Agent（已启动但无法注册）
                        if let Some(ref handles) = session_handles {
                            if let Some(ref lifecycle) = handles.lifecycle_handle {
                                lifecycle.cancel(); // 立即取消
                            }
                        }
                        // 发送错误响应
                        if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                            project_id: project_id.clone(),
                            session_id: String::new(),
                            code: shared_types::error_codes::ERR_TOO_MANY_REQUESTS.to_string(),
                            error: Some(format!("系统繁忙：并发 Agent 会话数已达上限 ({} 个)", limit)),
                            request_id: Some(request_id.clone()),
                            service_type: request.prompt_message.service_type.clone(),
                        }) {
                            error!("❌ 发送拒绝响应失败: {:?}", send_err);
                        }
                        return;
                    }
                }

                if let Some(ref handles) = session_handles {
                    debug!("🆕 新会话，注册到 AGENT_REGISTRY");

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
                // 复用现有会话
                // 检查 gRPC 层是否错误地预留了槽位（极少见）
                let grpc_predicted_new_session = predict_needs_new_session_sync(
                    &project_id,
                    &request.prompt_message.session_id,
                );

                if grpc_predicted_new_session {
                    // 预测错误：gRPC 预测新会话但实际复用了现有会话，需要释放槽位
                    warn!(
                        "⚠️ [原子槽位] 预测错误（gRPC预测新会话但实际复用），释放预留槽位 - project_id={}",
                        project_id
                    );
                    AGENT_REGISTRY.release_session_slot();
                }
                debug!("♻️ 复用会话，无需获取新槽位（Agent 已占用槽位）");
            }

            // 7. 更新 SESSION_REQUEST_CONTEXT（请求追踪）
            SESSION_REQUEST_CONTEXT.insert(project_id.clone(), request_id.clone());

            // 8. 转换并发送回执
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
                error!("❌ 发送回执失败: {:?}", e);
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
                        // 这确保了槽位释放和 Registry 清理的原子性
                        // 后续的 stop_agent 或 cleanup_task 调用 remove_by_project() 会发现 Agent 已不存在
                        // 因此不会重复释放槽位
                        AGENT_REGISTRY.remove_by_project(&project_id);

                        info!(
                            "🛑 [SACP] Agent 生命周期结束，已清理 Registry 并释放槽位 - project_id={}, session_id={}",
                            project_id, response_session_id
                        );
                    } else {
                        warn!(
                            "⚠️ [SACP] 新会话缺少 lifecycle_handle - project_id={}",
                            project_id
                        );
                        // 缺少 lifecycle_handle，立即清理
                        AGENT_REGISTRY.remove_by_project(&project_id);
                    }
                } else {
                    // 🔥 边界情况：is_new_session = true 但 session_handles = None
                    // 这种情况下 Agent 没有被注册，但可能已经预留了槽位
                    // 检查 gRPC 层是否预留了槽位
                    let grpc_predicted_new_session = predict_needs_new_session_sync(
                        &project_id,
                        &request.prompt_message.session_id,
                    );

                    if grpc_predicted_new_session {
                        error!(
                            "❌ [SACP] 新会话缺少 session_handles，释放预留槽位 - project_id={}, session_id={}",
                            project_id, response_session_id
                        );
                        AGENT_REGISTRY.release_session_slot();
                    } else {
                        warn!(
                            "⚠️ [SACP] 新会话缺少 session_handles - project_id={}, session_id={}",
                            project_id, response_session_id
                        );
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

    info!("🛑 agent_worker 停止");
    Ok(())
}

/// 预测是否需要创建新会话
///
/// 复用 session_manager 的判断逻辑，避免在启动 Agent 后才发现槽位已满
///
/// ## 返回值
///
/// - `true`: 需要创建新会话（需要预留槽位）
/// - `false`: 可以复用现有会话（不需要槽位）
pub(crate) async fn predict_needs_new_session(
    prompt_message: &agent_abstraction::PromptMessage,
    model_provider: &Option<shared_types::ModelProviderConfig>,
) -> bool {
    use shared_types::SessionEntry;

    let project_id = &prompt_message.project_id;

    // 1. 检查 session_id_hint（如果提供）
    if let Some(ref session_id_hint) = prompt_message.session_id {
        if let Some(existing) = AGENT_REGISTRY.get_agent_info_by_session(session_id_hint) {
            // 验证 project_id 是否匹配
            if existing.project_id() == project_id {
                let channel_closed = existing.is_channel_closed();
                let model_changed = existing.is_model_config_changed(model_provider);

                if !channel_closed && !model_changed {
                    info!(
                        "🔄 [PREDICT] 通过 session_id_hint 可复用会话: project_id={}, session_id={}",
                        project_id, session_id_hint
                    );
                    return false; // 复用现有会话
                }
            }
        }
    }

    // 2. 检查 project_id 是否已有会话
    if let Some(existing) = AGENT_REGISTRY.get_agent_info(project_id) {
        // 🔥 关键修复：显式检查 Pending 状态
        // Pending 状态意味着会话正在被创建中（由 PendingGuard 创建的占位符）
        // 这种情况下应该预测需要新会话（实际上会替换占位符）
        if *existing.status() == AgentStatus::Pending {
            info!(
                "🆕 [PREDICT] 检测到 Pending 占位符，预测需要新会话（将替换占位符）: project_id={}",
                project_id
            );
            return true; // 需要创建新会话（实际上会替换 Pending 占位符）
        }

        let channel_closed = existing.is_channel_closed();
        let model_changed = existing.is_model_config_changed(model_provider);

        if !channel_closed && !model_changed {
            info!(
                "🔄 [PREDICT] 通过 project_id 可复用会话: project_id={}",
                project_id
            );
            return false; // 复用现有会话
        }

        // Channel 关闭或模型变化，需要重建
        info!(
            "🆕 [PREDICT] 现有会话无效，需要重建: project_id={}, channel_closed={}, model_changed={}",
            project_id, channel_closed, model_changed
        );
    }

    // 3. 需要创建新会话
    info!(
        "🆕 [PREDICT] 需要创建新会话: project_id={}",
        project_id
    );
    true
}

// ============================================================================
// 单元测试：验证提前并发检查
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use agent_abstraction::PromptMessage;
    use shared_types::{ModelProviderConfig, ServiceType};

    /// 创建测试用的 PromptMessage
    fn create_test_prompt_message(
        project_id: &str,
        session_id: Option<String>,
    ) -> PromptMessage {
        PromptMessage {
            project_id: project_id.to_string(),
            project_path: std::path::PathBuf::from(format!("/tmp/{}", project_id)),
            content: "test prompt".to_string(),
            request_id: format!("req-{}", project_id),
            session_id,
            system_prompt_override: None,
            user_prompt_template_override: None,
            agent_config_override: None,
            attachments: vec![],
            data_source_attachments: vec![],
            service_type: ServiceType::ComputerAgentRunner,
        }
    }

    /// 创建测试用的 ProjectAndAgentInfo
    /// 返回 (ProjectAndAgentInfo, PromptReceiver, CancelReceiver) 来确保 channel 保持打开
    fn create_test_agent_info(project_id: &str, session_id: &str) -> (ProjectAndAgentInfo, tokio::sync::mpsc::Receiver<sacp::schema::PromptRequest>, tokio::sync::mpsc::Receiver<shared_types::CancelNotificationRequestWrapper>) {
        use sacp::schema::PromptRequest;
        use shared_types::CancelNotificationRequestWrapper;
        let (prompt_tx, prompt_rx) = mpsc::channel::<PromptRequest>(100);
        let (cancel_tx, cancel_rx) = mpsc::channel::<CancelNotificationRequestWrapper>(100);

        let agent_info = ProjectAndAgentInfo {
            project_id: project_id.to_string(),
            session_id: SessionId::new(Arc::from(session_id)),
            prompt_tx,
            cancel_tx,
            model_provider: None,
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            stop_handle: None,
        };

        (agent_info, prompt_rx, cancel_rx)
    }

    #[tokio::test]
    async fn test_predict_needs_new_session_no_existing_session() {
        let project_id = "test-new-session";

        // 预测：无现有会话，应该返回 true（需要新会话）
        let prompt_message = create_test_prompt_message(project_id, None);
        let needs_new = predict_needs_new_session(&prompt_message, &None).await;

        assert!(needs_new, "无现有会话时，应该预测需要新会话");
    }

    #[tokio::test]
    async fn test_predict_reuse_via_session_id_hint() {
        let project_id = "test-reuse-session";
        let session_id = "session-123";

        // 创建现有会话，保留 receiver 以保持 channel 打开
        let (agent_info, _prompt_rx, _cancel_rx) = create_test_agent_info(project_id, session_id);
        AGENT_REGISTRY.register(project_id, session_id, agent_info);

        // 预测：通过 session_id_hint 可以复用
        let prompt_message = create_test_prompt_message(project_id, Some(session_id.to_string()));
        let needs_new = predict_needs_new_session(&prompt_message, &None).await;

        assert!(!needs_new, "通过 session_id_hint 可复用时，应该预测不需要新会话");

        // 清理
        AGENT_REGISTRY.remove_by_project(project_id);
    }

    #[tokio::test]
    async fn test_predict_reuse_via_project_id() {
        let project_id = "test-reuse-project";
        let session_id = "session-456";

        // 创建现有会话，保留 receiver 以保持 channel 打开
        let (agent_info, _prompt_rx, _cancel_rx) = create_test_agent_info(project_id, session_id);
        AGENT_REGISTRY.register(project_id, session_id, agent_info);

        // 预测：通过 project_id 可以复用（无 session_id_hint）
        let prompt_message = create_test_prompt_message(project_id, None);
        let needs_new = predict_needs_new_session(&prompt_message, &None).await;

        assert!(!needs_new, "通过 project_id 可复用时，应该预测不需要新会话");

        // 清理
        AGENT_REGISTRY.remove_by_project(project_id);
    }

    #[tokio::test]
    async fn test_predict_rebuild_when_channel_closed() {
        let project_id = "test-rebuild-channel";
        let session_id = "session-789";

        // 创建现有会话，然后关闭 channel
        use sacp::schema::PromptRequest;
        use shared_types::CancelNotificationRequestWrapper;

        // 创建一个 helper 函数来生成已关闭的 channel
        fn create_closed_sender<T>() -> mpsc::Sender<T> {
            let (tx, _) = mpsc::channel::<T>(100);
            tx
        }
        // 立即 drop 它来关闭 channel
        let _closed = create_closed_sender::<PromptRequest>();

        let (cancel_tx, _) = mpsc::channel::<CancelNotificationRequestWrapper>(100);

        let agent_info = ProjectAndAgentInfo {
            project_id: project_id.to_string(),
            session_id: SessionId::new(Arc::from(session_id)),
            prompt_tx: create_closed_sender::<PromptRequest>(),
            cancel_tx,
            model_provider: None,
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            stop_handle: None,
        };

        AGENT_REGISTRY.register(project_id, session_id, agent_info);

        // 预测：channel 已关闭，需要重建
        let prompt_message = create_test_prompt_message(project_id, None);
        let needs_new = predict_needs_new_session(&prompt_message, &None).await;

        assert!(needs_new, "channel 关闭时，应该预测需要新会话");

        // 清理
        AGENT_REGISTRY.remove_by_project(project_id);
    }

    #[tokio::test]
    async fn test_predict_rebuild_when_model_changed() {
        let project_id = "test-rebuild-model";
        let session_id = "session-999";

        // 创建现有会话，使用模型 A，保留 receiver 以保持 channel 打开
        let model_a = Some(ModelProviderConfig {
            id: "model-a".to_string(),
            name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            api_key: "sk-test-key-a".to_string(),
            requires_openai_auth: false,
            default_model: "claude-3-5-sonnet".to_string(),
            api_protocol: Some("anthropic".to_string()),
        });

        let (mut agent_info, _prompt_rx, _cancel_rx) = create_test_agent_info(project_id, session_id);
        agent_info.model_provider = model_a.clone();
        AGENT_REGISTRY.register(project_id, session_id, agent_info);

        // 预测：使用模型 B，需要重建
        let model_b = Some(ModelProviderConfig {
            id: "model-b".to_string(),
            name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            api_key: "sk-test-key-b".to_string(),
            requires_openai_auth: false,
            default_model: "claude-3-5-opus".to_string(),
            api_protocol: Some("anthropic".to_string()),
        });

        let prompt_message = create_test_prompt_message(project_id, None);
        let needs_new = predict_needs_new_session(&prompt_message, &model_b).await;

        assert!(needs_new, "模型配置变化时，应该预测需要新会话");

        // 清理
        AGENT_REGISTRY.remove_by_project(project_id);
    }

    #[tokio::test]
    async fn test_predict_no_model_change() {
        let project_id = "test-no-model-change";
        let session_id = "session-111";

        // 创建现有会话，保留 receiver 以保持 channel 打开
        let model = Some(ModelProviderConfig {
            id: "model-same".to_string(),
            name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            api_key: "sk-test-key-same".to_string(),
            requires_openai_auth: false,
            default_model: "claude-3-5-sonnet".to_string(),
            api_protocol: Some("anthropic".to_string()),
        });

        let (mut agent_info, _prompt_rx, _cancel_rx) = create_test_agent_info(project_id, session_id);
        agent_info.model_provider = model.clone();
        AGENT_REGISTRY.register(project_id, session_id, agent_info);

        // 预测：使用相同的模型，可以复用
        let prompt_message = create_test_prompt_message(project_id, None);
        let needs_new = predict_needs_new_session(&prompt_message, &model).await;

        assert!(!needs_new, "模型配置未变化时，应该预测可以复用");

        // 清理
        AGENT_REGISTRY.remove_by_project(project_id);
    }

    #[tokio::test]
    async fn test_predict_session_id_hint_mismatched_project() {
        let project_a = "project-a";
        let project_b = "project-b";
        let session_id = "session-222";

        // 为 project_a 创建会话，保留 receiver 以保持 channel 打开
        let (agent_info, _prompt_rx, _cancel_rx) = create_test_agent_info(project_a, session_id);
        AGENT_REGISTRY.register(project_a, session_id, agent_info);

        // 预测：project_b 使用 session_id_hint，但 session 属于 project_a
        let prompt_message = create_test_prompt_message(project_b, Some(session_id.to_string()));
        let needs_new = predict_needs_new_session(&prompt_message, &None).await;

        assert!(needs_new, "session_id 属于不同 project 时，应该预测需要新会话");

        // 清理
        AGENT_REGISTRY.remove_by_project(project_a);
    }
}
