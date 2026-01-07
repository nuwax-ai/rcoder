//! SACP Agent Worker 实现
//!
//! 使用 SACP 协议的 Agent Worker，支持标准 `tokio::spawn`。
//!
//! ## Feature Flag
//!
//! 此模块通过 `sacp` feature 启用。

use std::sync::Arc;

use agent_config::{AgentServersConfig, PromptConfigAssembler};
use anyhow::Result;
use shared_types::{ProjectAndAgentInfo, SessionEntry};
use tracing::{debug, error, info};

use super::{AgentWorker, SessionHandles, WorkerRequest, WorkerResponse};
use super::sacp_session_manager::SacpSessionManager;
use crate::launcher::convert_context_servers_sacp;
use crate::traits::{AgentStartConfig, SessionNotifier, SessionRegistry};

/// SACP Agent Worker
///
/// 使用 SACP 协议的 Agent Worker 实现，支持标准 `tokio::spawn`。
///
/// # 类型参数
/// - `N`: SessionNotifier 实现，用于推送 SSE 消息
/// - `R`: SessionRegistry 实现，用于存储会话数据
pub struct SacpAgentWorker<N: SessionNotifier + 'static, R: SessionRegistry> {
    session_manager: Arc<SacpSessionManager<N, R>>,
}

impl<N: SessionNotifier + 'static, R: SessionRegistry> SacpAgentWorker<N, R>
where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    /// 创建新的 SACP Agent Worker
    pub fn new(session_manager: Arc<SacpSessionManager<N, R>>) -> Self {
        Self { session_manager }
    }
}

#[async_trait::async_trait]
impl<N: SessionNotifier + Send + Sync + 'static, R: SessionRegistry + Send + Sync + 'static>
    AgentWorker for SacpAgentWorker<N, R>
where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    fn name(&self) -> &'static str {
        "SacpAgentWorker"
    }

    async fn process_request(&self, request: WorkerRequest) -> Result<WorkerResponse> {
        let project_id = request.project_id().to_string();
        let project_path = request.project_path().clone();

        info!("[SACP] SacpAgentWorker 处理请求，project_id: {}", project_id);

        // 1. 路径规范化
        let normalized_path = SacpSessionManager::<N, R>::normalize_path(&project_path);
        debug!("[SACP] 路径规范化: {:?}", normalized_path);

        // 2. 确保项目目录存在
        SacpSessionManager::<N, R>::ensure_project_dir(&normalized_path)
            .await
            .map_err(|e| {
                error!("[SACP] 创建项目目录失败: {:?}", e);
                e
            })?;

        // 3. 使用 PromptConfigAssembler 组装配置
        let default_agent_id = "claude-code-acp";
        let servers_config =
            AgentServersConfig::load_or_default_for_service(&request.prompt_message.service_type)
                .await;

        let assembler = PromptConfigAssembler::new(servers_config)
            .with_system_prompt(request.prompt_message.system_prompt_override.clone())
            .with_user_prompt_template(request.prompt_message.user_prompt_template_override.clone())
            .with_agent_config(request.prompt_message.agent_config_override.clone());

        // 获取最终的系统提示词
        let system_prompt = assembler.get_system_prompt(default_agent_id);
        // 应用用户提示词模板
        let final_user_prompt =
            assembler.apply_user_prompt(default_agent_id, &request.prompt_message.content);

        info!(
            "[SACP] 提示词处理 - 系统提示词: has_override={}, length={} | 用户提示词: has_template={}, original_len={}, final_len={}",
            assembler.has_system_prompt_override(),
            system_prompt.len(),
            assembler.has_user_prompt_template_override(),
            request.prompt_message.content.len(),
            final_user_prompt.len()
        );

        // 获取 MCP 服务器配置
        let context_servers = assembler.get_context_servers();
        debug!(
            "[SACP] MCP 服务器配置: has_override={}, count={}",
            assembler.has_agent_config_override(),
            context_servers.len()
        );

        // 将 context_servers 转换为 SACP 协议的 McpServer 格式
        let mcp_servers = convert_context_servers_sacp(&context_servers);
        debug!("[SACP] 转换后的 MCP 服务器数量: {}", mcp_servers.len());

        // 构建 AgentStartConfig
        let mut start_config = AgentStartConfig::new(request.prompt_message.service_type.clone())
            .with_system_prompt(system_prompt)
            .with_mcp_servers(mcp_servers);

        // Resume 策略
        if let Some(ref session_id) = request.prompt_message.session_id {
            info!("[SACP] 用户传入 session_id，尝试 resume: {}", session_id);
            start_config = start_config.with_resume_session_id(session_id.clone());
        }

        // 4. 更新 prompt_message 的 content 为处理后的用户提示词
        let mut prompt_message = request.prompt_message.clone();
        prompt_message.content = final_user_prompt;

        // 5. 获取或创建会话（使用 SACP SessionManager）
        let (session_entry, is_new) = self
            .session_manager
            .get_or_create_session(
                &project_id,
                normalized_path,
                prompt_message.session_id.clone(),
                request.model_provider.clone(),
                start_config.clone(),
                request.service_uuid.clone(),
            )
            .await
            .map_err(|e| {
                error!("[SACP] 获取或创建会话失败: {:?}", e);
                e
            })?;

        info!(
            "[SACP] 会话已准备，session_id: {}, is_new: {}",
            session_entry.session_id().0,
            is_new
        );

        // 6. 构建 Prompt 请求
        let prompt_request = if let Some(ref attachment_blocks) = request.attachment_blocks {
            debug!("[SACP] 构建带附件的 Prompt 请求");
            SacpSessionManager::<N, R>::build_prompt_request_with_attachments(
                &prompt_message,
                session_entry.session_id().clone(),
                attachment_blocks.clone(),
            )?
        } else {
            debug!("[SACP] 构建纯文本 Prompt 请求");
            SacpSessionManager::<N, R>::build_text_prompt_request(
                &prompt_message,
                session_entry.session_id().clone(),
            )?
        };

        // 7. 发送 Prompt
        self.session_manager
            .send_prompt_request(&project_id, prompt_request)
            .map_err(|e| {
                error!("[SACP] 发送 Prompt 请求失败: {:?}", e);
                e
            })?;

        info!("[SACP] Prompt 请求已发送，project_id: {}", project_id);

        // 8. 构建响应
        if is_new {
            Ok(WorkerResponse::new_session_success(
                project_id,
                session_entry.session_id().0.to_string(),
                Some(request.request_id().to_string()),
                prompt_message.service_type.clone(),
                SessionHandles {
                    prompt_tx: session_entry.prompt_tx().clone(),
                    cancel_tx: session_entry.cancel_tx().clone(),
                    lifecycle_handle: session_entry.lifecycle_handle().cloned(),
                },
            ))
        } else {
            Ok(WorkerResponse::reuse_session_success(
                project_id,
                session_entry.session_id().0.to_string(),
                Some(request.request_id().to_string()),
                prompt_message.service_type.clone(),
            ))
        }
    }
}
