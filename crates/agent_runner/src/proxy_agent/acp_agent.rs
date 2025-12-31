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
use tracing::{debug, error, info};

use crate::{
    model::{AgentStatus, ChatPromptResponse, ProjectAndAgentInfo},
    proxy_agent::{AcpAgentClient, SESSION_REQUEST_CONTEXT},
    service::{AGENT_REGISTRY, AgentSessionRegistry, StateAwareNotifier},
    utils::ContentBuilder,
};

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
        } else {
            info!(
                "✅ 回执已发送，project_id: {}",
                request.prompt_message.project_id
            );
        }
    }

    info!("🛑 agent_worker 停止");
    Ok(())
}
