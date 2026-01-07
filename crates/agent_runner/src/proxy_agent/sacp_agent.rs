//! SACP Agent Worker 模块
//!
//! 使用 SACP 协议的 Agent Worker，支持标准 `tokio::spawn`（无需 LocalSet）。
//!
//! ## Feature Flag
//!
//! 此模块通过 `sacp` feature 启用。

use std::sync::Arc;

use agent_abstraction::session::{AgentWorker, SacpAgentWorker, SacpSessionManager, WorkerRequest};
use anyhow::Result;
use dashmap::DashMap;
use shared_types::ModelProviderConfig;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

use crate::{
    model::ChatPromptResponse,
    proxy_agent::SESSION_REQUEST_CONTEXT,
    service::{AgentSessionRegistry, StateAwareNotifier, AGENT_REGISTRY},
    utils::ContentBuilder,
};

/// SACP 版本的 Agent 请求（无需 LocalSet）
#[derive(Debug)]
pub struct SacpAgentRequest {
    /// Agent 抽象层的 prompt 消息
    pub prompt_message: agent_abstraction::PromptMessage,
    /// 发送回执消息的通道
    pub chat_prompt_tx: oneshot::Sender<ChatPromptResponse>,
    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,
    /// 关联的 service UUID
    pub service_uuid: Option<String>,
    /// 共享的 API 密钥管理器
    pub shared_api_key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
}

impl SacpAgentRequest {
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

    pub fn with_service_uuid(mut self, service_uuid: Option<String>) -> Self {
        self.service_uuid = service_uuid;
        self
    }

    pub fn with_key_manager(
        mut self,
        key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
    ) -> Self {
        self.shared_api_key_manager = key_manager;
        self
    }
}

/// SACP Agent Worker 任务
///
/// 使用标准 `tokio::spawn`，无需 LocalSet。
/// 这是旧版 `agent_worker_with_heartbeat` 的 SACP 替代版本。
pub async fn sacp_agent_worker(
    mut request_rx: mpsc::UnboundedReceiver<SacpAgentRequest>,
) -> Result<()> {
    info!("🚀 [SACP] sacp_agent_worker 启动（无 LocalSet），开始监听请求...");

    // 创建 SacpSessionManager
    let session_manager = Arc::new(SacpSessionManager::<
        StateAwareNotifier,
        AgentSessionRegistry,
    >::new(
        Arc::new(StateAwareNotifier::new()),
        AGENT_REGISTRY.clone(),
    ));

    // 创建 SacpAgentWorker
    let worker = SacpAgentWorker::new(session_manager);

    while let Some(request) = request_rx.recv().await {
        let project_id = request.prompt_message.project_id.clone();
        let request_id = request.prompt_message.request_id.clone();

        info!(
            "[SACP] 接收到请求，project_id: {}, request_id: {}",
            project_id, request_id
        );

        // 1. 预处理附件
        let attachment_blocks = if !request.prompt_message.attachments.is_empty() {
            match ContentBuilder::attachments_to_content_blocks(
                &request.prompt_message.attachments,
                &request.prompt_message.project_path,
            )
            .await
            {
                Ok(blocks) => Some(blocks),
                Err(e) => {
                    error!("[SACP] 附件处理失败: {:?}", e);
                    if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                        project_id: project_id.clone(),
                        session_id: String::new(),
                        code: shared_types::error_codes::ERR_AGENT_ERROR.to_string(),
                        error: Some(format!("附件处理失败: {:?}", e)),
                        request_id: Some(request_id),
                        service_type: request.prompt_message.service_type.clone(),
                    }) {
                        error!("[SACP] 发送错误响应失败: {:?}", send_err);
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

        // 3. 调用 SacpAgentWorker 处理
        let worker_response = match worker.process_request(worker_request).await {
            Ok(response) => response,
            Err(e) => {
                error!("[SACP] Worker 处理失败: {:?}", e);
                if let Err(send_err) = request.chat_prompt_tx.send(ChatPromptResponse {
                    project_id: project_id.clone(),
                    session_id: String::new(),
                    code: shared_types::error_codes::ERR_AGENT_ERROR.to_string(),
                    error: Some(format!("处理失败: {:?}", e)),
                    request_id: Some(request_id.clone()),
                    service_type: request.prompt_message.service_type.clone(),
                }) {
                    error!("[SACP] 发送错误响应失败: {:?}", send_err);
                }
                continue;
            }
        };

        // 4. 日志记录（注册已在 SacpSessionManager::create_session 中完成）
        if worker_response.is_new_session {
            info!(
                "[SACP] 新会话已创建: project_id={}, session_id={}",
                project_id, worker_response.session_id
            );
        } else {
            debug!("[SACP] 复用现有会话: project_id={}, session_id={}",
                project_id, worker_response.session_id
            );
        }

        // 5. 更新 SESSION_REQUEST_CONTEXT
        SESSION_REQUEST_CONTEXT.insert(project_id.clone(), request_id.clone());

        // 6. 发送回执
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
            error!("[SACP] 发送回执失败: {:?}", e);
        } else {
            info!(
                "[SACP] 回执已发送，project_id: {}",
                request.prompt_message.project_id
            );
        }
    }

    info!("[SACP] sacp_agent_worker 停止");
    Ok(())
}
