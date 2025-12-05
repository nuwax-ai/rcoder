//! ACP Agent Worker 模块
//!
//! 负责处理 Agent 请求队列，管理 Agent 会话的创建和复用。
//! 使用 AcpSessionManager 进行会话管理。

use std::sync::{Arc, LazyLock};

use agent_abstraction::session::AcpSessionManager;
use agent_client_protocol::{PromptRequest, SessionId};
use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use shared_types::ModelProviderConfig;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

use crate::{
    model::{AgentStatus, ChatPromptResponse, ProjectAndAgentInfo},
    proxy_agent::{AcpAgentClient, SESSION_REQUEST_CONTEXT},
    service::StateAwareNotifier,
    utils::ContentBuilder,
};

/// 全局 ProjectAndAgentInfo 映射
///
/// 保留此全局变量以保持与现有 HTTP API 的兼容性。
/// 内部使用 AcpSessionManager 进行会话管理。
pub static PROJECT_AND_AGENT_INFO_MAP: LazyLock<DashMap<String, ProjectAndAgentInfo>> =
    LazyLock::new(DashMap::new);

/// LocalSet 中运行的 Agent 请求
#[derive(Debug)]
pub struct LocalSetAgentRequest {
    /// Agent 抽象层的 prompt 消息
    prompt_message: agent_abstraction::PromptMessage,
    /// 发送回执消息的通道
    chat_prompt_tx: oneshot::Sender<ChatPromptResponse>,
    /// 模型提供商配置
    model_provider: Option<ModelProviderConfig>,
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
            },
            chat_prompt_rx,
        )
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

    // 创建 AcpSessionManager
    let session_manager = Arc::new(
        AcpSessionManager::<StateAwareNotifier, AcpAgentClient>::new(Arc::new(
            StateAwareNotifier::new(),
        )),
    );

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
                    let _ = request.chat_prompt_tx.send(ChatPromptResponse {
                        project_id: project_id.clone(),
                        session_id: String::new(),
                        error: Some(format!("附件处理失败: {:?}", e)),
                        request_id: Some(request_id),
                        service_type: request.prompt_message.service_type.clone(),
                    });
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
        };

        // 3. 调用 AcpAgentWorker 处理（核心业务逻辑）
        let worker_response = match worker.process_request(worker_request).await {
            Ok(response) => response,
            Err(e) => {
                error!("❌ Worker 处理失败: {:?}", e);
                let _ = request.chat_prompt_tx.send(ChatPromptResponse {
                    project_id: project_id.clone(),
                    session_id: String::new(),
                    error: Some(format!("处理失败: {:?}", e)),
                    request_id: Some(request_id.clone()),
                    service_type: request.prompt_message.service_type.clone(),
                });
                continue;
            }
        };

        // 4. 更新全局状态（HTTP API 兼容性）
        if worker_response.is_new_session {
            if let Some(handles) = &worker_response.session_handles {
                debug!("🆕 新会话，更新 PROJECT_AND_AGENT_INFO_MAP");

                let project_and_agent_info = ProjectAndAgentInfo {
                    project_id: project_id.clone(),
                    session_id: SessionId::new(Arc::from(worker_response.session_id.as_str())),
                    prompt_tx: handles.prompt_tx.clone(),
                    cancel_tx: handles.cancel_tx.clone(),
                    model_provider: request.model_provider.clone(),
                    request_id: Some(request_id.clone()),
                    status: AgentStatus::Idle,
                    last_activity: Utc::now(),
                    created_at: Utc::now(),
                    stop_handle: handles.lifecycle_handle.clone(),
                };

                PROJECT_AND_AGENT_INFO_MAP.insert(project_id.clone(), project_and_agent_info);

                // 建立 project_id -> session_id 映射
                let cleared = crate::service::ensure_project_session(
                    &project_id,
                    &worker_response.session_id,
                )
                .await;

                if cleared > 0 {
                    info!(
                        "🧹 Project session 映射更新，已清理旧消息: project_id={}, cleared_count={}",
                        project_id, cleared
                    );
                } else {
                    info!(
                        "🔗 Project session 映射已同步: project_id={}, session_id={}",
                        project_id, worker_response.session_id
                    );
                }
            }
        } else {
            debug!("♻️ 复用会话，无需更新全局 MAP");
        }

        // 5. 更新 SESSION_REQUEST_CONTEXT（请求追踪）
        SESSION_REQUEST_CONTEXT.insert(project_id, request_id.clone());

        // 6. 转换并发送回执
        let chat_prompt_response = ChatPromptResponse {
            project_id: worker_response.project_id,
            session_id: worker_response.session_id,
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

/// 构建 Prompt 请求
///
/// 使用 ACP 协议分离模式：
/// - 系统提示词已在 NewSessionRequest._meta.systemPrompt 中传递
/// - 这里只构建纯用户提示词和附件
pub async fn build_prompt_to_acp_agent(
    prompt: agent_abstraction::PromptMessage,
    session_id: SessionId,
) -> Result<PromptRequest> {
    use agent_client_protocol::{ContentBlock, TextContent};
    use agent_config::PromptBuilder;

    // 构建纯用户提示词
    let final_prompt = if prompt.data_source_attachments.is_empty() {
        PromptBuilder::new().build_user_prompt(&prompt.content)
    } else {
        PromptBuilder::new()
            .build_user_prompt_with_data_sources(&prompt.content, &prompt.data_source_attachments)
    };

    // 创建文本内容块
    let text_block = ContentBlock::Text(TextContent::new(final_prompt));
    let mut content_blocks = vec![text_block];

    // 如果有附件，转换为内容块
    if !prompt.attachments.is_empty() {
        let attachment_blocks = ContentBuilder::attachments_to_content_blocks(
            &prompt.attachments,
            &prompt.project_path,
        )
        .await?;
        content_blocks.extend(attachment_blocks);
    }

    // 将 request_id 放入 meta 字段
    debug!(
        "🔧 [build_prompt] 将 request_id={} 放入 PromptRequest.meta",
        prompt.request_id
    );
    let mut meta = serde_json::Map::new();
    meta.insert(
        "request_id".to_string(),
        serde_json::Value::String(prompt.request_id),
    );

    Ok(PromptRequest::new(session_id, content_blocks).meta(meta))
}
