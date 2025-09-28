mod acp_agent;
pub mod cancel_handler;
mod channel_utils;
pub mod cleanup_task;
mod claude_code_agent;
mod codex_agent;

pub use acp_agent::{LocalSetAgentRequest, PROJECT_AND_AGENT_INFO_MAP, agent_worker};
pub use cancel_handler::{CancelHandler, AgentCleanupHandler, ClaudeCodeHandler, CodexHandler};
pub use cleanup_task::{AgentCleaner, CleanupConfig, CleanupCommand, CleanupStats, start_cleanup_task};
use agent_client_protocol::{
    Client, PermissionOptionKind, PromptRequest, SessionId,
};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::{service::push_session_update, model::{SessionNotify, AgentSessionUpdate}};
use crate::CancelNotificationRequest;

/// ACP协议的连接信息
pub struct AcpConnectionInfo {
    /// 会话ID
    pub session_id: SessionId,
    /// 用于发送 Prompt 的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// 用于发送取消通知的通道
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequest>,
}

/// ACP 客户端实现[derive(Clone)]
pub struct AcpAgentClient;

#[async_trait::async_trait(?Send)]
impl Client for AcpAgentClient {
    async fn request_permission(
        &self,
        args: agent_client_protocol::RequestPermissionRequest,
    ) -> Result<agent_client_protocol::RequestPermissionResponse, agent_client_protocol::Error> {
        debug!("请求权限: {:?}", args);
        // 自动允许：优先选择 AllowAlways，其次 AllowOnce；若都无，选第一个选项
        let selected = args
            .options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowAlways)
            .or_else(|| {
                args.options
                    .iter()
                    .find(|o| o.kind == PermissionOptionKind::AllowOnce)
            })
            .or_else(|| args.options.first());
        if let Some(option) = selected {
            return Ok(agent_client_protocol::RequestPermissionResponse {
                outcome: agent_client_protocol::RequestPermissionOutcome::Selected {
                    option_id: option.id.clone(),
                },
                meta: None,
            });
        }
        // 无可选项则取消
        Ok(agent_client_protocol::RequestPermissionResponse {
            outcome: agent_client_protocol::RequestPermissionOutcome::Cancelled,
            meta: None,
        })
    }

    async fn write_text_file(
        &self,
        args: agent_client_protocol::WriteTextFileRequest,
    ) -> Result<agent_client_protocol::WriteTextFileResponse, agent_client_protocol::Error> {
        debug!("写入文件: {:?}", args);
        if let Some(parent) = std::path::Path::new(&args.path).parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                error!("创建目录失败: {}", e);
                agent_client_protocol::Error::internal_error()
            })?;
        }
        let mut file = tokio::fs::File::create(&args.path).await.map_err(|e| {
            error!("创建文件失败: {}", e);
            agent_client_protocol::Error::internal_error()
        })?;
        file.write_all(args.content.as_bytes()).await.map_err(|e| {
            error!("写入文件失败: {}", e);
            agent_client_protocol::Error::internal_error()
        })?;
        Ok(agent_client_protocol::WriteTextFileResponse { meta: None })
    }

    async fn read_text_file(
        &self,
        args: agent_client_protocol::ReadTextFileRequest,
    ) -> Result<agent_client_protocol::ReadTextFileResponse, agent_client_protocol::Error> {
        debug!("读取文件: {:?}", args);
        let content = tokio::fs::read_to_string(&args.path).await.map_err(|e| {
            error!("读取文件失败: {}", e);
            agent_client_protocol::Error::internal_error()
        })?;
        Ok(agent_client_protocol::ReadTextFileResponse {
            content,
            meta: None,
        })
    }

    async fn create_terminal(
        &self,
        _request: agent_client_protocol::CreateTerminalRequest,
    ) -> Result<agent_client_protocol::CreateTerminalResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _request: agent_client_protocol::TerminalOutputRequest,
    ) -> Result<agent_client_protocol::TerminalOutputResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _request: agent_client_protocol::ReleaseTerminalRequest,
    ) -> Result<agent_client_protocol::ReleaseTerminalResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _request: agent_client_protocol::WaitForTerminalExitRequest,
    ) -> Result<agent_client_protocol::WaitForTerminalExitResponse, agent_client_protocol::Error>
    {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _request: agent_client_protocol::KillTerminalCommandRequest,
    ) -> Result<agent_client_protocol::KillTerminalCommandResponse, agent_client_protocol::Error>
    {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn session_notification(
        &self,
        args: agent_client_protocol::SessionNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        let session_id_str = args.session_id.to_string();

        // 将SessionUpdate转换为SessionNotify并存入全局缓存
        // 尝试从 PROJECT_AND_AGENT_INFO_MAP 中找到对应的 request_id
        // 由于 ACP 回调只提供 session_id，我们需要遍历查找对应的 project_id
        let request_id = PROJECT_AND_AGENT_INFO_MAP.iter()
            .find(|entry| entry.value().session_id.to_string() == session_id_str)
            .map(|entry| entry.value().request_id.clone())
            .unwrap_or(None);

        let agent_update = AgentSessionUpdate {
            session_id: session_id_str.clone(),
            session_update: args.update.clone(),
            request_id,
        };
        let notify = SessionNotify::AgentSessionUpdate(agent_update);
        if let Err(e) = push_session_update(&session_id_str, notify) {
            error!(
                "❌ Failed to cache SessionUpdate for session {}: {}",
                session_id_str, e
            );
        }

        // 记录日志（保持原有的详细日志）
        match &args.update {
            agent_client_protocol::SessionUpdate::AgentMessageChunk { content } => {
                let text = match content {
                    agent_client_protocol::ContentBlock::Text(text_content) => text_content.text.clone(),
                    agent_client_protocol::ContentBlock::Image(_) => "<image>".into(),
                    agent_client_protocol::ContentBlock::Audio(_) => "<audio>".into(),
                    agent_client_protocol::ContentBlock::ResourceLink(resource_link) => {
                        resource_link.uri.clone()
                    }
                    agent_client_protocol::ContentBlock::Resource(_) => "<resource>".into(),
                };
                info!("📥 Agent message cached [session:{}]: {}", session_id_str, text);
            }
            _ => {
                info!(
                    "📥 SessionUpdate cached [session:{}]: {:?}",
                    session_id_str, args.update
                );
            }
        }

        Ok(())
    }

    async fn ext_method(
        &self,
        _request: agent_client_protocol::ExtRequest,
    ) -> Result<agent_client_protocol::ExtResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn ext_notification(
        &self,
        _notification: agent_client_protocol::ExtNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }
}
