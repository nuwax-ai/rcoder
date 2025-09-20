//! Codex AI Agent Library
//!
//! 提供基于 OpenAI Codex 的 AI 代理服务，通过 ACP (Agent Client Protocol) 协议实现
//! 与 OpenAI API 的集成。

pub mod agent;

pub use agent::{CodexAgent, CodexConfig, ClientOp};

use agent_client_protocol::{
    Agent, AgentCapabilities, AuthMethod, AuthMethodId, InitializeRequest, InitializeResponse,
    AuthenticateRequest, AuthenticateResponse, NewSessionRequest, NewSessionResponse,
    LoadSessionRequest, LoadSessionResponse, SetSessionModeRequest, SetSessionModeResponse,
    PromptRequest, PromptResponse, CancelNotification, ExtRequest, ExtResponse,
    ExtNotification, SessionNotification, Error
};
use tokio::sync::{mpsc, oneshot::Sender};
use std::sync::Arc;

/// Codex 代理工厂
pub struct CodexAgentFactory;

impl CodexAgentFactory {
    /// 创建新的 Codex 代理实例
    pub fn create_agent(
        config: CodexConfig,
        session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
        client_tx: mpsc::UnboundedSender<ClientOp>,
    ) -> Arc<dyn Agent> {
        Arc::new(CodexAgent::with_config(
            session_update_tx,
            client_tx,
            config,
        ))
    }

    /// 创建默认配置的 Codex 代理
    pub fn create_default_agent(
        session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
        client_tx: mpsc::UnboundedSender<ClientOp>,
    ) -> Arc<dyn Agent> {
        Self::create_agent(
            CodexConfig::default(),
            session_update_tx,
            client_tx,
        )
    }

    /// 检查 Codex 是否可用
    pub fn is_available() -> bool {
        // 检查环境变量或 OpenAI API Key 是否可用
        std::env::var("OPENAI_API_KEY").is_ok()
    }

    /// 获取支持的认证方法
    pub fn supported_auth_methods() -> Vec<AuthMethod> {
        vec![
            AuthMethod {
                id: AuthMethodId("chatgpt".into()),
                name: "ChatGPT".into(),
                description: Some("Sign in with ChatGPT to use your plan".into()),
                meta: None,
            },
            AuthMethod {
                id: AuthMethodId("apikey".into()),
                name: "OpenAI API Key".into(),
                description: Some("Use OPENAI_API_KEY from environment or auth.json".into()),
                meta: None,
            },
        ]
    }
}

/// Codex 代理构建器
pub struct CodexAgentBuilder {
    config: CodexConfig,
}

impl CodexAgentBuilder {
    /// 创建新的构建器
    pub fn new() -> Self {
        Self {
            config: CodexConfig::default(),
        }
    }

    /// 设置工作目录
    pub fn with_cwd(mut self, cwd: std::path::PathBuf) -> Self {
        self.config.cwd = cwd;
        self
    }

    /// 设置 Codex 主目录
    pub fn with_codex_home(mut self, codex_home: std::path::PathBuf) -> Self {
        self.config.codex_home = codex_home;
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
        CodexAgentFactory::create_agent(self.config, session_update_tx, client_tx)
    }
}

impl Default for CodexAgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_default(),
            codex_home: dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".codex"),
            model: "gpt-4".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codex_agent_factory() {
        // 测试工厂方法
        assert!(!CodexAgentFactory::supported_auth_methods().is_empty());
    }

    #[test]
    fn test_codex_agent_builder() {
        let builder = CodexAgentBuilder::new()
            .with_model("gpt-4".to_string());
        
        assert_eq!(builder.config.model, "gpt-4");
    }

    #[test]
    fn test_codex_config_default() {
        let config = CodexConfig::default();
        assert_eq!(config.model, "gpt-4");
    }
}