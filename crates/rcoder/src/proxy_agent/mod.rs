mod acp_agent;
mod claude_code_agent;
mod codex_agent;

pub use acp_agent::{LocalSetAgentRequest, PROJECT_AND_AGENT_INFO_MAP, agent_worker};
use agent_client_protocol::{self as acp};
use agent_client_protocol::{Client, PermissionOptionKind};
use tokio::io::AsyncWriteExt as _;
use tracing::{debug, error, info};

/// ACP 客户端实现[derive(Clone)]
pub struct AcpAgentClient;

#[async_trait::async_trait(?Send)]
impl Client for AcpAgentClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
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
            return Ok(acp::RequestPermissionResponse {
                outcome: agent_client_protocol::RequestPermissionOutcome::Selected {
                    option_id: option.id.clone(),
                },
                meta: None,
            });
        }
        // 无可选项则取消
        Ok(acp::RequestPermissionResponse {
            outcome: agent_client_protocol::RequestPermissionOutcome::Cancelled,
            meta: None,
        })
    }

    async fn write_text_file(
        &self,
        args: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileResponse, acp::Error> {
        debug!("写入文件: {:?}", args);
        if let Some(parent) = std::path::Path::new(&args.path).parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                error!("创建目录失败: {}", e);
                acp::Error::internal_error()
            })?;
        }
        let mut file = tokio::fs::File::create(&args.path).await.map_err(|e| {
            error!("创建文件失败: {}", e);
            acp::Error::internal_error()
        })?;
        file.write_all(args.content.as_bytes()).await.map_err(|e| {
            error!("写入文件失败: {}", e);
            acp::Error::internal_error()
        })?;
        Ok(acp::WriteTextFileResponse { meta: None })
    }

    async fn read_text_file(
        &self,
        args: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, acp::Error> {
        debug!("读取文件: {:?}", args);
        let content = tokio::fs::read_to_string(&args.path).await.map_err(|e| {
            error!("读取文件失败: {}", e);
            acp::Error::internal_error()
        })?;
        Ok(acp::ReadTextFileResponse {
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
        //TODO 需要实现
        match args.update {
            acp::SessionUpdate::AgentMessageChunk { content } => {
                let text = match content {
                    acp::ContentBlock::Text(text_content) => text_content.text,
                    acp::ContentBlock::Image(_) => "<image>".into(),
                    acp::ContentBlock::Audio(_) => "<audio>".into(),
                    acp::ContentBlock::ResourceLink(resource_link) => resource_link.uri,
                    acp::ContentBlock::Resource(_) => "<resource>".into(),
                };
                info!("| Agent session_notification : {text}");
            }
            acp::SessionUpdate::UserMessageChunk { .. }
            | acp::SessionUpdate::AgentThoughtChunk { .. }
            | acp::SessionUpdate::ToolCall(_)
            | acp::SessionUpdate::ToolCallUpdate(_)
            | acp::SessionUpdate::Plan(_)
            | acp::SessionUpdate::CurrentModeUpdate { .. }
            | acp::SessionUpdate::AvailableCommandsUpdate { .. } => {
                info!("| Other session_notification: {:?}", args.update);
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
