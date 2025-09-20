//! Claude AI Agent Library
//!
//! 提供基于 Claude Code 的 AI 代理服务，通过 ACP (Agent Client Protocol) 协议实现
//! 与 Claude Code CLI 工具的集成。

pub mod agent;

pub use agent::{ClaudeAgent, ClaudeConfig, ApprovalPolicy, TokenUsage, ClientOp};

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthMethod, AuthMethodId, InitializeRequest, InitializeResponse,
    AuthenticateRequest, AuthenticateResponse, NewSessionRequest, NewSessionResponse,
    LoadSessionRequest, LoadSessionResponse, SetSessionModeRequest, SetSessionModeResponse,
    PromptRequest, PromptResponse, CancelNotification, ExtRequest, ExtResponse,
    ExtNotification, SessionNotification, Error
};
use tokio::sync::{mpsc, oneshot::Sender};
use std::sync::Arc;

/// Claude 代理工厂
pub struct ClaudeAgentFactory;

impl ClaudeAgentFactory {
    /// 创建新的 Claude 代理实例
    pub fn create_agent(
        config: ClaudeConfig,
        session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
        client_tx: mpsc::UnboundedSender<ClientOp>,
    ) -> Arc<dyn Agent> {
        Arc::new(ClaudeAgent::with_config(
            session_update_tx,
            client_tx,
            config,
        ))
    }

    /// 创建默认配置的 Claude 代理
    pub fn create_default_agent(
        session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
        client_tx: mpsc::UnboundedSender<ClientOp>,
    ) -> Arc<dyn Agent> {
        Self::create_agent(
            ClaudeConfig::default(),
            session_update_tx,
            client_tx,
        )
    }

    /// 检查 Claude 是否可用
    pub fn is_available() -> bool {
        // 检查环境变量或 Claude Code CLI 是否可用
        std::env::var("CLAUDE_API_KEY").is_ok()
    }

    /// 获取支持的认证方法
    pub fn supported_auth_methods() -> Vec<AuthMethod> {
        vec![
            AuthMethod {
                id: AuthMethodId("claude".into()),
                name: "Claude".into(),
                description: Some("Sign in with Claude Code".into()),
                meta: None,
            },
            AuthMethod {
                id: AuthMethodId("api_key".into()),
                name: "API Key".into(),
                description: Some("Use CLAUDE_API_KEY from environment".into()),
                meta: None,
            },
        ]
    }
}

/// Claude 代理构建器
pub struct ClaudeAgentBuilder {
    config: ClaudeConfig,
}

impl ClaudeAgentBuilder {
    /// 创建新的构建器
    pub fn new() -> Self {
        Self {
            config: ClaudeConfig::default(),
        }
    }

    /// 设置工作目录
    pub fn with_cwd(mut self, cwd: std::path::PathBuf) -> Self {
        self.config.cwd = cwd;
        self
    }

    /// 设置 Claude 主目录
    pub fn with_claude_home(mut self, claude_home: std::path::PathBuf) -> Self {
        self.config.claude_home = claude_home;
        self
    }

    /// 设置模型
    pub fn with_model(mut self, model: String) -> Self {
        self.config.model = model;
        self
    }

    /// 构建代理
    pub fn build(
        self,
        session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
        client_tx: mpsc::UnboundedSender<ClientOp>,
    ) -> Arc<dyn Agent> {
        ClaudeAgentFactory::create_agent(self.config, session_update_tx, client_tx)
    }
}

impl Default for ClaudeAgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_agent_factory() {
        // 测试工厂方法
        assert!(!ClaudeAgentFactory::supported_auth_methods().is_empty());
    }

    #[test]
    fn test_claude_agent_builder() {
        let builder = ClaudeAgentBuilder::new()
            .with_model("claude-3-5-sonnet-20241022".to_string());
        
        assert_eq!(builder.config.model, "claude-3-5-sonnet-20241022");
    }
}