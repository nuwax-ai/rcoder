use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol::schema::{PromptRequest, ProtocolVersion, SessionId};
use agent_config::ContextServerConfig;
use tokio::sync::mpsc;

use super::super::lifecycle::AgentLifecycleGuard;
use crate::acp::CancelNotificationRequestWrapper;

/// 使用最新协议版本
pub(crate) const VERSION: ProtocolVersion = ProtocolVersion::LATEST;

/// 环境变量键名常量
pub(crate) const ENV_ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
pub(crate) const ENV_ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
pub(crate) const ENV_ANTHROPIC_MODEL: &str = "ANTHROPIC_MODEL";
pub(crate) const ENV_DISABLE_NONESSENTIAL: &str = "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC";
pub(crate) const ENV_AGENT_SDK_SKIP_VERSION_CHECK: &str = "CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK";
pub(crate) const ENV_RUST_LOG: &str = "RUST_LOG";
pub(crate) const ENV_AGENT_WORKING_DIR: &str = "AGENT_WORKING_DIR";
pub(crate) const ENV_AGENT_PROJECT_ID: &str = "AGENT_PROJECT_ID";

/// OpenAI 环境变量常量
pub(crate) const ENV_OPENAI_API_KEY: &str = "OPENAI_API_KEY";
pub(crate) const ENV_OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";
/// nuwaxcode 使用 OPENCODE_MODEL 而不是 OPENAI_MODEL
pub(crate) const ENV_OPENCODE_MODEL: &str = "OPENCODE_MODEL";

/// Codex 环境变量常量（nuwax-codex-acp 使用这些变量）
pub(crate) const ENV_CODEX_API_KEY: &str = "CODEX_API_KEY";
pub(crate) const ENV_CODEX_BASE_URL: &str = "CODEX_BASE_URL";

/// Agent 配置参数 (与旧版兼容)
#[derive(Debug, Clone)]
pub struct SacpAgentLaunchConfig {
    /// 命令路径
    pub command: String,
    /// 命令参数
    pub args: Vec<String>,
    /// 环境变量
    pub env: HashMap<String, String>,
    /// Context 服务器配置 (MCP servers)
    pub context_servers: HashMap<String, ContextServerConfig>,
}

/// Agent 连接信息（SACP 版本）
pub struct SacpLauncherConnectionInfo {
    /// 会话 ID
    pub session_id: SessionId,
    /// 发送 Prompt 消息的通道（有界通道，提供背压保护）
    pub prompt_tx: mpsc::Sender<PromptRequest>,
    /// 发送取消请求的通道（有界通道，提供背压保护）
    pub cancel_tx: mpsc::Sender<CancelNotificationRequestWrapper>,
    /// 生命周期守卫（自动清理资源）
    pub lifecycle_guard: Arc<AgentLifecycleGuard>,
}
