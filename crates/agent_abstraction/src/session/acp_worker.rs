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

        info!("📨 AcpAgentWorker 处理请求，project_id: {}", project_id);

        // 1. 路径规范化
        let normalized_path = AcpSessionManager::<N, R>::normalize_path(&project_path);
        debug!("📂 路径规范化: {:?}", normalized_path);

        // 2. 确保项目目录存在
        AcpSessionManager::<N, R>::ensure_project_dir(&normalized_path)
            .await
            .map_err(|e| {
                error!("❌ 创建项目目录失败: {:?}", e);
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
        debug!("🎯 使用的 Agent ID: {}", agent_id);

        // 获取最终的系统提示词（入参有值则使用入参，否则使用默认配置）
        // 🆕 使用实际 agent_id 而不是 default_agent_id
        let system_prompt = assembler.get_system_prompt(&agent_id);
        // 应用用户提示词模板（如果有）
        // 🆕 使用实际 agent_id 而不是 default_agent_id
        let final_user_prompt =
            assembler.apply_user_prompt(&agent_id, &request.prompt_message.content);

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

        // 构建 AgentStartConfig 并传递 MCP 服务器、service_type
        let mut start_config = AgentStartConfig::new(request.prompt_message.service_type.clone())
            .with_system_prompt(system_prompt)
            .with_mcp_servers(mcp_servers);

        // 🆕 如果用户指定了 agent_server 配置，添加到 start_config
        // 注意：这里直接使用用户传入的配置，由 launcher 层负责与默认配置合并
        if let Some(ref override_config) = request.prompt_message.agent_config_override {
            if let Some(ref agent_server) = override_config.agent_server {
                debug!(
                    "📝 使用用户指定的 Agent 服务器配置: command={:?}, args={:?}",
                    agent_server.command, agent_server.args
                );
                start_config = start_config.with_agent_server_override(agent_server.clone());
            }
        }

        // Resume 策略：验证 session 文件是否存在，再决定是否 resume
        //
        // 工作原理：
        // 1. 用户传入 session_id → 检查磁盘上是否存在 session 文件
        // 2. 如果存在 → 设置 resume_session_id → Agent 恢复上下文
        // 3. 如果不存在 → 不设置 resume → Agent 创建新会话
        //
        // Session 文件路径: ~/.claude/projects/{encoded_path}/{session_id}.jsonl
        //
        // 优势：
        // - 提前验证，避免传入无效 session_id 导致 Agent 启动失败
        // - 服务重启后能恢复磁盘上的会话
        // - 不依赖内存状态，更健壮
        // - 自动降级（有会话就恢复，没有就创建）
        if let Some(ref session_id) = request.prompt_message.session_id {
            // 检查 session 文件是否存在（使用文件系统扫描 + 缓存）
            let project_path_str = normalized_path.to_string_lossy().to_string();
            let session_exists =
                super::check_session_file_exists(session_id, &project_path_str).await;

            if session_exists {
                info!("✅ Session 文件存在，尝试 resume: {}", session_id);
                start_config = start_config.with_resume_session_id(session_id.clone());
            } else {
                warn!(
                    "⚠️ Session 文件不存在，跳过 resume，将创建新会话: session_id={}",
                    session_id
                );
                // 不设置 resume_session_id，Agent 将创建新会话
            }
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
                error!("❌ 获取或创建会话失败: {:?}", e);
                e
            })?;

        // 使用 SessionEntry trait 方法访问会话信息
        info!(
            "✅ 会话已准备，session_id: {}, is_new: {}",
            session_entry.session_id(),
            is_new
        );

        // 6. 构建 Prompt 请求
        let prompt_request = if let Some(ref attachment_blocks) = request.attachment_blocks {
            debug!("📎 构建带附件的 Prompt 请求");
            AcpSessionManager::<N, R>::build_prompt_request_with_attachments(
                &prompt_message,
                session_entry.session_id().clone(),
                attachment_blocks.clone(),
            )?
        } else {
            debug!("📝 构建纯文本 Prompt 请求");
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
                error!("❌ 发送 Prompt 请求失败: {:?}", e);
                e
            })?;

        info!("✅ Prompt 请求已发送，project_id: {}", project_id);

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
