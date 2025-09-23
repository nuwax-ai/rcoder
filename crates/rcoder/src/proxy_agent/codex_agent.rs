use agent_client_protocol::{self as acp, Client, PromptRequest};
use tracing::info;

/// 嵌入式客户端实现
#[derive(Clone)]
pub struct EmbeddedCodexClient;

#[async_trait::async_trait(?Send)]
impl Client for EmbeddedCodexClient {
    async fn request_permission(
        &self,
        _request: agent_client_protocol::RequestPermissionRequest,
    ) -> Result<agent_client_protocol::RequestPermissionResponse, agent_client_protocol::Error>
    {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn write_text_file(
        &self,
        _request: agent_client_protocol::WriteTextFileRequest,
    ) -> Result<agent_client_protocol::WriteTextFileResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _request: agent_client_protocol::ReadTextFileRequest,
    ) -> Result<agent_client_protocol::ReadTextFileResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
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
