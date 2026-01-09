//! ACP Agent Worker 模块
//!
//! 负责处理 Agent 请求队列，管理 Agent 会话的创建和复用。
//! 使用 AcpSessionManager 进行会话管理。

use std::sync::Arc;

use dashmap::DashMap;

use agent_abstraction::session::AcpSessionManager;
use anyhow::Result;
use chrono::Utc;
use shared_types::ModelProviderConfig;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::{
    agent_worker_manager::{Heartbeat, WORKER_THREAD_POOL_SIZE, WorkerHandle, WorkerReady},
    model::{AgentStatus, ChatPromptResponse, ProjectAndAgentInfo},
    proxy_agent::{AcpAgentClient, SESSION_REQUEST_CONTEXT},
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

/// LocalSet 中运行的 Agent 请求
#[derive(Debug)]
pub struct LocalSetAgentRequest {
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

impl LocalSetAgentRequest {
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

/// Agent Worker 任务（简化版）
///
/// 在本地线程中运行，处理 Agent 请求队列。
/// 使用 AcpAgentWorker 处理核心业务逻辑。
pub async fn agent_worker(
    mut request_rx: mpsc::UnboundedReceiver<LocalSetAgentRequest>,
) -> Result<()> {
    use agent_abstraction::session::{AcpAgentWorker, AgentWorker, WorkerRequest};
    use agent_client_protocol::SessionId;

    info!("🚀 agent_worker 启动（简化版），开始监听请求...");

    // 创建 AcpSessionManager，注入 AGENT_REGISTRY 作为 SessionRegistry
    let session_manager = Arc::new(AcpSessionManager::<
        StateAwareNotifier,
        AcpAgentClient,
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

/// 🔥 新增：带心跳的 agent_worker
///
/// 这是新的入口点，支持心跳和自动重启监控
/// 保留原 `agent_worker` 函数以兼容现有代码
pub async fn agent_worker_with_heartbeat(
    mut request_rx: mpsc::UnboundedReceiver<LocalSetAgentRequest>,
    mut handle: WorkerHandle,
) -> Result<()> {
    info!("🚀 agent_worker 启动（带心跳支持），开始监听请求...");

    use agent_abstraction::session::{AcpAgentWorker, AgentWorker, WorkerRequest};
    use agent_client_protocol::SessionId;
    use tokio::time::{Duration, interval};

    // 创建 AcpSessionManager，注入 AGENT_REGISTRY 作为 SessionRegistry
    let session_manager = Arc::new(AcpSessionManager::<
        StateAwareNotifier,
        AcpAgentClient,
        AgentSessionRegistry,
    >::new(
        Arc::new(StateAwareNotifier::new()),
        AGENT_REGISTRY.clone(),
    ));

    // 创建 AcpAgentWorker
    let worker = AcpAgentWorker::new(session_manager);

    // 🆕 发送 Worker 就绪信号（仅在启动时发送一次，oneshot）
    if let Some(ready_tx) = handle.ready_tx.take() {
        let ready_signal = WorkerReady {
            timestamp: Utc::now(),
        };
        if let Err(_e) = ready_tx.send(ready_signal) {
            warn!("⚠️ [Worker] Ready 信号发送失败（接收端已关闭）");
        } else {
            info!("✅ [Worker] Ready 信号已发送，LocalSet 初始化完成");
        }
    }

    // 🔥 启动心跳任务（独立 spawn，不在 LocalSet 中）
    let heartbeat_tx = handle.heartbeat_tx.clone();
    let heartbeat_task = tokio::spawn(async move {
        let mut heartbeat_interval = interval(Duration::from_secs(5));
        loop {
            heartbeat_interval.tick().await;
            let heartbeat = Heartbeat {
                timestamp: Utc::now(),
            };

            if let Err(e) = heartbeat_tx.try_send(heartbeat) {
                warn!("⚠️ [Worker] 心跳发送失败: {}", e);
                // 如果监控任务已关闭，worker 也应该退出
                break;
            }
        }
    });

    // 🆕 主处理循环 - 改为并发处理，每个请求在独立的 spawn_blocking 中运行
    while let Some(request) = request_rx.recv().await {
        let project_id = request.prompt_message.project_id.clone();
        let request_id = request.prompt_message.request_id.clone();

        info!(
            "📨 接收到请求，project_id: {}, request_id: {} - 准备并发处理",
            project_id, request_id
        );

        // 克隆需要的变量，用于 spawn 任务
        let worker_clone = worker.clone();
        let handle_clone = handle.clone();

        // 🚀 使用 spawn_blocking 为每个请求创建独立的阻塞任务
        // 因为 LocalSet 不是 Send，需要在专用的 blocking 线程中运行
        tokio::task::spawn_blocking(move || {
            // 在 blocking 线程中创建一个单线程运行时
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create runtime for request");

            rt.block_on(async move {
                // 创建独立的 LocalSet，用于运行 !Send 的 ACP 连接
                let local_set = tokio::task::LocalSet::new();

                local_set.run_until(async move {
                info!(
                    "🔵 [LocalSet] 开始处理请求 project_id={}, request_id={}",
                    project_id, request_id
                );

                // 🔥 OpenTelemetry 追踪: 创建请求 span
                let _otel_span = RequestSpan::new(&project_id, &request_id, "process_agent_request");

                // 🔥 并发控制: 检查活跃 Agent 会话数（基于 AGENT_REGISTRY）
                let active_sessions_count = AGENT_REGISTRY.stats().agent_count;

                // 🛡️ 并发限制: 会话数达到工作线程池上限时直接拒绝
                if active_sessions_count >= WORKER_THREAD_POOL_SIZE {
                    error!(
                        "🛡️ [并发限制] Agent 会话数已达上限 ({}/{}), 拒绝新请求 - project_id={}, request_id={}",
                        active_sessions_count, WORKER_THREAD_POOL_SIZE, project_id, request_id
                    );

                    // 🔥 关键修复：清理 Pending 状态，避免状态泄漏
                    AGENT_REGISTRY.clear_pending_if_exists(&project_id);

                    if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                        project_id: project_id.clone(),
                        session_id: String::new(),
                        code: shared_types::error_codes::ERR_TOO_MANY_REQUESTS.to_string(),
                        error: Some(format!(
                            "系统繁忙：并发 Agent 会话数已达上限 ({} 个)，请稍后重试",
                            WORKER_THREAD_POOL_SIZE
                        )),
                        request_id: Some(request_id.clone()),
                        service_type: request.prompt_message.service_type.clone(),
                    }) {
                        error!("❌ 发送拒绝响应失败（接收端已关闭）: {:?}", send_err);
                    }
                    return; // 退出当前 LocalSet
                }

                // 📊 日志: 记录当前会话数（低于上限时）
                debug!(
                    "✅ [并发检查通过] 当前活跃会话数: {}/{}, project_id={}, request_id={}",
                    active_sessions_count, WORKER_THREAD_POOL_SIZE, project_id, request_id
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

                            // 🔥 关键修复：清理 Pending 状态，避免状态泄漏
                            AGENT_REGISTRY.clear_pending_if_exists(&project_id);

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
                            return; // 退出当前 LocalSet
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
                        error!("❌ Worker 处理失败: {:?}", e);

                        // 🔥 关键修复：清理 Pending 状态，避免状态泄漏
                        AGENT_REGISTRY.clear_pending_if_exists(&project_id);

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
                        return; // 退出当前 LocalSet
                    }
                };

                // 4. 提取 session_handles（在移动 worker_response 之前）
                // 🔥 关键：需要保存 lifecycle_handle 用于后续等待会话结束
                let session_handles = worker_response.session_handles.clone();
                let is_new_session = worker_response.is_new_session;
                let response_session_id = worker_response.session_id.clone();

                // 5. 更新全局状态（使用统一的 AGENT_REGISTRY）
                if is_new_session {
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
                    debug!("♻️ 复用会话，无需更新全局 Registry");
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
                    error!("❌ 发送回执失败: {:?}", e);
                } else {
                    info!(
                        "✅ 回执已发送，project_id: {}",
                        request.prompt_message.project_id
                    );
                }

                // 🔥🔥🔥 关键修复：对于新会话，保持 LocalSet 存活直到会话结束 🔥🔥🔥
                //
                // 问题背景：
                // - launch() 中使用 spawn_local() 创建了 Prompt 处理器
                // - spawn_local 任务依赖 LocalSet 存活才能运行
                // - 如果 run_until 在 process_request() 完成后立即退出，LocalSet 会被销毁
                // - 导致 Prompt 处理器被终止，无法接收和处理消息
                //
                // 解决方案：
                // - 对于新会话，等待 cancel_token 被取消后才退出 run_until
                // - 这样 LocalSet 会一直存活，Prompt 处理器可以持续工作
                // - 对于复用会话，原会话的 LocalSet 仍在运行，无需额外等待
                if is_new_session {
                    if let Some(ref handles) = session_handles {
                        if let Some(ref lifecycle) = handles.lifecycle_handle {
                            info!(
                                "🔄 [LocalSet] 新会话已启动，保持 LocalSet 存活等待会话结束 - project_id={}, session_id={}",
                                project_id, response_session_id
                            );

                            // 等待 Agent 会话结束（通过 cancellation_token）
                            // 当用户取消会话、会话超时或 Agent 完成工作时，cancellation_token 会被触发
                            lifecycle.cancellation_token().cancelled().await;

                            info!(
                                "🛑 [LocalSet] Agent 会话已结束，LocalSet 即将退出 - project_id={}, session_id={}",
                                project_id, response_session_id
                            );
                        } else {
                            warn!(
                                "⚠️ [LocalSet] 新会话缺少 lifecycle_handle，无法等待会话结束 - project_id={}",
                                project_id
                            );
                        }
                    }
                } else {
                    info!(
                        "🔵 [LocalSet] 复用会话请求处理完成，LocalSet 退出 - project_id={}, request_id={}",
                        project_id, request_id
                    );
                }
                }).await;
            })
        });

        // 立即继续循环，接收下一个请求 - 不等待上面的 spawn_blocking 完成
    }

    // 清理心跳任务
    heartbeat_task.abort();

    info!("🛑 agent_worker 停止");
    Ok(())
}
