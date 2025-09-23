use acp_adapter::SessionId;
use agent_client_protocol::{Client, PromptRequest};
use tokio::sync::mpsc;

/// 使用Agent代理的工具类型,都是使用ACP协议包装过的agent代理
pub enum AgentType {
    /// OpenAI Codex 代理
    Codex,
    /// Claude Code 代理
    Claude,
}

/// 项目id与 Agent 服务池，一个项目对应一个 Agent 服务
#[derive(Clone)]
pub struct ProjectAndAgentInfo {
    /// 项目ID
    pub project_id: String,
    /// 会话ID,agent 服务启动时会创建一个会话ID
    pub session_id: SessionId,
    /// 用于发送 Prompt 的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
}

/// 嵌入式客户端实现
#[derive(Clone)]
pub struct EmbeddedClient;

#[async_trait::async_trait(?Send)]
impl Client for EmbeddedClient {
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
        _notification: agent_client_protocol::SessionNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        //TODO 需要实现
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
