//! ACP Agent Worker 实现
//!
//! 封装 Agent 请求处理的核心业务逻辑

use std::sync::Arc;

use agent_client_protocol::Client;
use agent_config::AgentServersConfig;
use anyhow::Result;
use async_trait::async_trait;
use tracing::{debug, error, info};

use super::{AcpSessionManager, AgentWorker, SessionHandles, WorkerRequest, WorkerResponse};
use crate::traits::{AgentStartConfig, SessionNotifier};

/// ACP Agent Worker
///
/// 处理 ACP Agent 请求的核心业务逻辑实现
pub struct AcpAgentWorker<N: SessionNotifier, C: Client + 'static> {
    session_manager: Arc<AcpSessionManager<N, C>>,
}

impl<N: SessionNotifier, C: Client + 'static> AcpAgentWorker<N, C> {
    /// 创建新的 ACP Agent Worker
    pub fn new(session_manager: Arc<AcpSessionManager<N, C>>) -> Self {
        Self { session_manager }
    }
}

#[async_trait::async_trait]
impl<N: SessionNotifier + Send + Sync + 'static, C: Client + Clone + Default + Send + Sync + 'static> AgentWorker
    for AcpAgentWorker<N, C>
{
    fn name(&self) -> &'static str {
        "AcpAgentWorker"
    }

    async fn process_request(&self, request: WorkerRequest) -> Result<WorkerResponse> {
        let project_id = request.project_id().to_string();
        let project_path = request.project_path().clone();

        info!("📨 AcpAgentWorker 处理请求，project_id: {}", project_id);

        // 1. 路径规范化
        let normalized_path = AcpSessionManager::<N, C>::normalize_path(&project_path);
        debug!("📂 路径规范化: {:?}", normalized_path);

        // 2. 确保项目目录存在
        AcpSessionManager::<N, C>::ensure_project_dir(&normalized_path)
            .await
            .map_err(|e| {
                error!("❌ 创建项目目录失败: {:?}", e);
                e
            })?;

        // 3. 准备 AgentStartConfig
        let servers_config = AgentServersConfig::load_or_default().await;
        let system_prompt = servers_config.get_system_prompt("claude-code-acp");
        let start_config = AgentStartConfig::new().with_system_prompt(system_prompt);

        // 4. 创建 Client 实例
        let client = C::default();

        // 5. 获取或创建会话
        let (session_info, is_new) = self
            .session_manager
            .get_or_create_session(
                &project_id,
                normalized_path,
                request.prompt_message.session_id.clone(),
                request.model_provider.clone(),
                start_config,
                client,
            )
            .await
            .map_err(|e| {
                error!("❌ 获取或创建会话失败: {:?}", e);
                e
            })?;

        info!(
            "✅ 会话已准备，session_id: {}, is_new: {}",
            session_info.session_id.0, is_new
        );

        // 6. 构建 Prompt 请求
        let prompt_request = if let Some(ref attachment_blocks) = request.attachment_blocks {
            debug!("📎 构建带附件的 Prompt 请求");
            AcpSessionManager::<N, C>::build_prompt_request_with_attachments(
                &request.prompt_message,
                session_info.session_id.clone(),
                attachment_blocks.clone(),
            )?
        } else {
            debug!("📝 构建纯文本 Prompt 请求");
            AcpSessionManager::<N, C>::build_text_prompt_request(
                &request.prompt_message,
                session_info.session_id.clone(),
            )?
        };

        // 7. 发送 Prompt
        self.session_manager
            .send_prompt_request(&project_id, prompt_request)
            .map_err(|e| {
                error!("❌ 发送 Prompt 请求失败: {:?}", e);
                e
            })?;

        info!("✅ Prompt 请求已发送，project_id: {}", project_id);

        // 8. 构建响应
        if is_new {
            Ok(WorkerResponse::new_session_success(
                project_id,
                session_info.session_id.0.to_string(),
                Some(request.request_id().to_string()),
                request.prompt_message.service_type.clone(),
                SessionHandles {
                    prompt_tx: session_info.prompt_tx.clone(),
                    cancel_tx: session_info.cancel_tx.clone(),
                    lifecycle_handle: session_info.lifecycle_handle.clone(),
                },
            ))
        } else {
            Ok(WorkerResponse::reuse_session_success(
                project_id,
                session_info.session_id.0.to_string(),
                Some(request.request_id().to_string()),
                request.prompt_message.service_type.clone(),
            ))
        }
    }
}
