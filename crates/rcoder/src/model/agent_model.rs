use acp_adapter::SessionId;
use agent_client_protocol::{self as acp, Client, PromptRequest};
use tokio::sync::mpsc;
use tracing::info;

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
