//! ACP Agent Worker 实现
//!
//! 封装 Agent 请求处理的核心业务逻辑

use std::sync::Arc;

use agent_client_protocol::Client;
use agent_config::{AgentServersConfig, PromptConfigAssembler};
use anyhow::Result;
use async_trait::async_trait;
use tracing::{debug, error, info};

use super::{AcpSessionManager, AgentWorker, SessionHandles, WorkerRequest, WorkerResponse};
use crate::compat::convert_context_servers;
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
impl<
    N: SessionNotifier + Send + Sync + 'static,
    C: Client + Clone + Default + Send + Sync + 'static,
> AgentWorker for AcpAgentWorker<N, C>
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

        // 3. 使用 PromptConfigAssembler 组装配置
        let default_agent_id = "claude-code-acp";
        // 根据请求中的 service_type 加载对应配置
        let servers_config =
            AgentServersConfig::load_or_default_for_service(&request.prompt_message.service_type)
                .await;

        let assembler = PromptConfigAssembler::new(servers_config)
            .with_system_prompt(request.prompt_message.system_prompt_override.clone())
            .with_user_prompt_template(request.prompt_message.user_prompt_template_override.clone())
            .with_agent_config(request.prompt_message.agent_config_override.clone());

        // 获取最终的系统提示词（入参有值则使用入参，否则使用默认配置）
        let system_prompt = assembler.get_system_prompt(default_agent_id);
        // 应用用户提示词模板（如果有）
        let final_user_prompt =
            assembler.apply_user_prompt(default_agent_id, &request.prompt_message.content);

        info!(
            "📝 提示词处理 - 系统提示词: has_override={}, length={} | 用户提示词: has_template={}, original_len={}, final_len={}",
            assembler.has_system_prompt_override(),
            system_prompt.len(),
            assembler.has_user_prompt_template_override(),
            request.prompt_message.content.len(),
            final_user_prompt.len()
        );

        debug!(
            "📝 系统提示词: has_override={}, length={}, content={}",
            assembler.has_system_prompt_override(),
            system_prompt.len(),
            system_prompt
        );
        debug!(
            "📝 用户提示词: has_template={}, original_len={}, final_len={}, final_content={}",
            assembler.has_user_prompt_template_override(),
            request.prompt_message.content.len(),
            final_user_prompt.len(),
            final_user_prompt
        );

        // 获取 MCP 服务器配置（入参有值则使用入参，否则使用默认配置）
        let context_servers = assembler.get_context_servers();
        debug!(
            "🔌 MCP 服务器配置: has_override={}, count={}",
            assembler.has_agent_config_override(),
            context_servers.len()
        );

        // 将 context_servers 转换为 ACP 协议的 McpServer 格式
        let mcp_servers = convert_context_servers(&context_servers);
        debug!("🔌 转换后的 MCP 服务器数量: {}", mcp_servers.len());

        // 构建 AgentStartConfig 并传递 MCP 服务器和 service_type
        let mut start_config = AgentStartConfig::new(request.prompt_message.service_type.clone())
            .with_system_prompt(system_prompt)
            .with_mcp_servers(mcp_servers);

        // TODO: 改进会话恢复策略
        //
        // 当前实现问题：
        // - 仅依赖内存中的 session_manager 判断会话是否存在
        // - 但 Claude Code 的会话是持久化到磁盘的（在 .claude/conversations/ 目录）
        // - 服务重启后，内存中会话信息丢失，但磁盘上的会话文件仍然存在
        // - 导致无法恢复磁盘上已有的对话历史
        //
        // 更好的设计（重试机制）：
        // 1. 如果提供了 session_id，先尝试使用 --resume 启动
        // 2. 如果 Claude Code 报错 "No conversation found"（会话不存在）
        // 3. 捕获错误，重试启动，这次不使用 --resume
        // 4. 这样既能恢复磁盘上的会话，又能避免首次创建时的崩溃
        //
        // 实现要点：
        // - 需要在 launcher 层捕获进程启动错误
        // - 解析 stderr 判断是否是 "No conversation found" 错误
        // - 实现重试逻辑（最多重试一次）
        // - 考虑性能影响（增加一次启动尝试）
        //
        // 优势：
        // - ✅ 服务重启后能恢复磁盘上的会话
        // - ✅ 不依赖内存状态，更健壮
        // - ✅ 自动降级（有会话就恢复，没有就创建）
        // - ✅ 用户体验更好（无需手动管理会话状态）

        // TODO: 改进会话恢复策略
        //
        // 当前实现（保守策略）：
        // - 仅当内存中存在会话且 session_id 匹配时，才使用 --resume
        // - 避免用户传入不存在的 session_id 导致 agent 恢复失败
        //
        // 后续优化（重试机制）：
        // - 先尝试使用 --resume 启动
        // - 如果 Claude Code 报错（会话不存在等），自动去掉 --resume 重试
        // - 这样可以自动恢复磁盘上的持久化会话（服务重启后）
        //
        // 当前暂不实现重试机制的原因：
        // - 错误发生在 new_session 成功之后（发送 prompt 时子进程才启动）
        // - launcher.launch() 已经返回成功，无法在 session_manager 层捕获错误
        // - 需要更复杂的架构改动来支持
        if let Some(ref session_id) = request.prompt_message.session_id {
            // 检查内存中是否存在这个 project 的会话，且 session_id 匹配
            if let Some(existing_session) = self.session_manager.get_session(&project_id) {
                let existing_session_id: &str = &existing_session.session_id.0;
                if existing_session_id == session_id {
                    debug!(
                        "✅ 会话存在且 session_id 匹配，使用 --resume: session_id={}",
                        session_id
                    );
                    start_config = start_config.with_resume_session_id(session_id.clone());
                } else {
                    debug!(
                        "⚠️ 会话存在但 session_id 不匹配（请求: {}, 已存在: {}），将创建新会话",
                        session_id, existing_session_id
                    );
                }
            } else {
                debug!(
                    "ℹ️ 会话不存在（内存），将创建新会话（不使用 --resume）: 请求的 session_id={}",
                    session_id
                );
            }
        }

        // 4. 创建 Client 实例
        let client = C::default();

        // 5. 更新 prompt_message 的 content 为处理后的用户提示词
        let mut prompt_message = request.prompt_message.clone();
        prompt_message.content = final_user_prompt;

        // 6. 获取或创建会话
        let (session_info, is_new) = self
            .session_manager
            .get_or_create_session(
                &project_id,
                normalized_path,
                prompt_message.session_id.clone(),
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

        // 7. 构建 Prompt 请求
        let prompt_request = if let Some(ref attachment_blocks) = request.attachment_blocks {
            debug!("📎 构建带附件的 Prompt 请求");
            AcpSessionManager::<N, C>::build_prompt_request_with_attachments(
                &prompt_message,
                session_info.session_id.clone(),
                attachment_blocks.clone(),
            )?
        } else {
            debug!("📝 构建纯文本 Prompt 请求");
            AcpSessionManager::<N, C>::build_text_prompt_request(
                &prompt_message,
                session_info.session_id.clone(),
            )?
        };

        // 8. 发送 Prompt
        self.session_manager
            .send_prompt_request(&project_id, prompt_request)
            .map_err(|e| {
                error!("❌ 发送 Prompt 请求失败: {:?}", e);
                e
            })?;

        info!("✅ Prompt 请求已发送，project_id: {}", project_id);

        // 9. 构建响应
        if is_new {
            Ok(WorkerResponse::new_session_success(
                project_id,
                session_info.session_id.0.to_string(),
                Some(request.request_id().to_string()),
                prompt_message.service_type.clone(),
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
                prompt_message.service_type.clone(),
            ))
        }
    }
}
