//! ACP Agent Worker 实现 (SACP 版本)
//!
//! 封装 Agent 请求处理的核心业务逻辑
//!
//! ## SACP 迁移说明
//!
//! - 移除了 `Client` trait 泛型参数（SACP 内部处理）
//! - 简化了会话创建参数

use std::sync::Arc;

use agent_config::{AgentServersConfig, PromptConfigAssembler};
use anyhow::Result;
use shared_types::{ProjectAndAgentInfo, SessionEntry};
use tracing::{debug, error, info, warn};

use super::{AcpSessionManager, AgentWorker, SessionHandles, WorkerRequest, WorkerResponse};
use crate::launcher::convert_context_servers;
use crate::traits::{AgentStartConfig, SessionNotifier, SessionRegistry};

/// ACP Agent Worker (SACP 版本)
///
/// 处理 ACP Agent 请求的核心业务逻辑实现
///
/// # 类型参数
/// - `N`: SessionNotifier 实现，用于推送 SSE 消息
/// - `R`: SessionRegistry 实现，用于存储会话数据
#[derive(Clone)]
pub struct AcpAgentWorker<N: SessionNotifier, R: SessionRegistry> {
    session_manager: Arc<AcpSessionManager<N, R>>,
}

impl<N: SessionNotifier + 'static, R: SessionRegistry> AcpAgentWorker<N, R> {
    /// 创建新的 ACP Agent Worker
    pub fn new(session_manager: Arc<AcpSessionManager<N, R>>) -> Self {
        Self { session_manager }
    }
}

#[async_trait::async_trait]
impl<N: SessionNotifier + Send + Sync + 'static, R: SessionRegistry + Send + Sync + 'static>
    AgentWorker for AcpAgentWorker<N, R>
where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    fn name(&self) -> &'static str {
        "AcpAgentWorker"
    }

    async fn process_request(&self, request: WorkerRequest) -> Result<WorkerResponse> {
        let project_id = request.project_id().to_string();
        let project_path = request.project_path().clone();

        info!(
            "📨 AcpAgentWorker received request, project_id: {}",
            project_id
        );

        // 1. 路径规范化
        let normalized_path = AcpSessionManager::<N, R>::normalize_path(&project_path);
        debug!("📂 path: {:?}", normalized_path);

        // 2. 确保项目目录存在
        AcpSessionManager::<N, R>::ensure_project_dir(&normalized_path)
            .await
            .map_err(|e| {
                error!("createdprojectdirectoryfailed: {:?}", e);
                e
            })?;

        // 3. 使用 PromptConfigAssembler 组装配置
        let default_agent_id = "claude-code-acp-ts";

        // 根据请求中的 service_type 加载对应配置
        let servers_config =
            AgentServersConfig::load_or_default_for_service(&request.prompt_message.service_type)
                .await;

        let assembler = PromptConfigAssembler::new(servers_config)
            .with_system_prompt(request.prompt_message.system_prompt_override.clone())
            .with_user_prompt_template(request.prompt_message.user_prompt_template_override.clone())
            .with_agent_config(request.prompt_message.agent_config_override.clone());

        // 🆕 获取用户指定的 agent_id（如果有）
        let agent_id = assembler.get_agent_id(default_agent_id);
        debug!("🎯 Resolved Agent ID: {}", agent_id);

        // 获取最终的系统提示词（入参有值则使用入参，否则使用默认配置）
        // 🆕 使用实际 agent_id 而不是 default_agent_id
        let system_prompt = assembler.get_system_prompt(&agent_id);
        // 应用用户提示词模板（如果有）
        // 🆕 使用实际 agent_id 而不是 default_agent_id
        let final_user_prompt =
            assembler.apply_user_prompt(&agent_id, &request.prompt_message.content);

        info!(
            "📝 Prompt processing - System prompt: has_override={}, length={} | User prompt: has_template={}, original_len={}, final_len={}",
            assembler.has_system_prompt_override(),
            system_prompt.len(),
            assembler.has_user_prompt_template_override(),
            request.prompt_message.content.len(),
            final_user_prompt.len()
        );

        debug!(
            "📝 System prompt: has_override={}, length={}, content={}",
            assembler.has_system_prompt_override(),
            system_prompt.len(),
            system_prompt
        );
        debug!(
            "📝 User prompt: has_template={}, original_len={}, final_len={}, final_content={}",
            assembler.has_user_prompt_template_override(),
            request.prompt_message.content.len(),
            final_user_prompt.len(),
            final_user_prompt
        );

        // 获取 MCP 服务器配置（入参有值则使用入参，否则使用默认配置）
        let context_servers = assembler.get_context_servers();
        debug!(
            "🔌 MCP server config: has_override={}, count={}",
            assembler.has_agent_config_override(),
            context_servers.len()
        );

        // 将 context_servers 转换为 ACP 协议的 McpServer 格式
        let mcp_servers = convert_context_servers(&context_servers);
        debug!("🔌 MCP servers: {}", mcp_servers.len());

        // 构建 AgentStartConfig 并传递 MCP 服务器、service_type
        let mut start_config = AgentStartConfig::new(request.prompt_message.service_type.clone())
            .with_system_prompt(system_prompt)
            .with_mcp_servers(mcp_servers)
            .with_user_id(request.prompt_message.user_id.clone());

        // 🆕 如果用户指定了 agent_server 配置，添加到 start_config
        // 注意：这里直接使用用户传入的配置，由 launcher 层负责与默认配置合并
        if let Some(ref override_config) = request.prompt_message.agent_config_override
            && let Some(ref agent_server) = override_config.agent_server
        {
            debug!(
                "📝 Using user-specified Agent server config: command={:?}, args={:?}",
                agent_server.command, agent_server.args
            );
            if let Err(err) = agent_server.agent_mode() {
                warn!(
                    "[ACP] Invalid agent_mode, falling back to yolo: project_id={}, error={}",
                    request.prompt_message.project_id, err
                );
            }
            start_config = start_config.with_agent_mode(
                agent_server
                    .agent_mode()
                    .unwrap_or(shared_types::AgentMode::Yolo),
            );
            start_config = start_config.with_agent_server_override(agent_server.clone());
        }

        // Resume 策略：直接传递 session_id，由 LoadSessionRequest 在 Agent 层面检查
        //
        // 工作原理：
        // 1. 用户传入 session_id → 直接设置 resume_session_id
        // 2. claude_code_sacp.rs 尝试 LoadSessionRequest 加载历史会话
        // 3. 如果 Agent 返回成功 → 恢复上下文
        // 4. 如果 Agent 返回错误/超时 → 降级到 NewSessionRequest 创建新会话
        //
        // 优势：
        // - Agent 层面统一处理，无需本地检查文件
        // - 与 Agent 行为保持一致
        // - 自动降级（有会话就恢复，没有就创建）
        if let Some(ref session_id) = request.prompt_message.session_id {
            info!("Setting resume_session_id: {}", session_id);
            start_config = start_config.with_resume_session_id(session_id.clone());
        }

        // 4. 更新 prompt_message 的 content 为处理后的用户提示词
        let mut prompt_message = request.prompt_message.clone();
        prompt_message.content = final_user_prompt;

        // 5. 获取或创建会话 (SACP 版本 - 不需要 client 和 shared_api_key_manager)
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
                error!("Failed to create session: {:?}", e);
                e
            })?;

        // 使用 SessionEntry trait 方法访问会话信息
        info!(
            "✅ Session ready, session_id: {}, is_new: {}",
            session_entry.session_id(),
            is_new
        );

        // 6. 构建 Prompt 请求
        let prompt_request = if let Some(ref attachment_blocks) = request.attachment_blocks {
            debug!("📎 Built Prompt request with attachments");
            AcpSessionManager::<N, R>::build_prompt_request_with_attachments(
                &prompt_message,
                session_entry.session_id().clone(),
                attachment_blocks.clone(),
            )?
        } else {
            debug!("📝 Built text Prompt request");
            AcpSessionManager::<N, R>::build_text_prompt_request(
                &prompt_message,
                session_entry.session_id().clone(),
            )?
        };

        // 7. 发送 Prompt（异步，支持背压）
        self.session_manager
            .send_prompt_request(&project_id, prompt_request)
            .await
            .map_err(|e| {
                error!("send Prompt requestfailed: {:?}", e);
                e
            })?;

        info!("Prompt request already sent, project_id: {}", project_id);

        // 8. 构建响应
        if is_new {
            Ok(WorkerResponse::new_session_success(
                project_id,
                session_entry.session_id().to_string(),
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
                session_entry.session_id().to_string(),
                Some(request.request_id().to_string()),
                prompt_message.service_type.clone(),
            ))
        }
    }
}
