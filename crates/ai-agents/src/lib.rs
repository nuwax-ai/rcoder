//! AI Agents 统一管理库
//!
//! 提供统一的接口来管理不同的 AI 代理（Claude、Codex 等），
//! 通过 Agent Client Protocol (ACP) 提供透明的访问。

use agent_client_protocol::{
    Agent, AuthMethod, SessionNotification, Error,
    InitializeRequest, InitializeResponse, AuthenticateRequest, AuthenticateResponse,
    NewSessionRequest, NewSessionResponse, LoadSessionRequest, LoadSessionResponse,
    SetSessionModeRequest, SetSessionModeResponse, PromptRequest, PromptResponse,
    CancelNotification, ExtRequest, ExtResponse, ExtNotification,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot::Sender};
use tracing::{info, warn};

/// AI 代理类型枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentType {
    /// Claude Code 代理
    Claude,
    /// OpenAI Codex 代理
    Codex,
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentType::Claude => write!(f, "claude"),
            AgentType::Codex => write!(f, "codex"),
        }
    }
}

impl std::str::FromStr for AgentType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "claude" => Ok(AgentType::Claude),
            "codex" => Ok(AgentType::Codex),
            _ => Err(format!("Unknown agent type: {}", s)),
        }
    }
}

/// AI 代理配置
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// 代理类型
    pub agent_type: AgentType,
    /// 工作目录
    pub cwd: std::path::PathBuf,
    /// 代理主目录
    pub home_dir: std::path::PathBuf,
    /// 使用的模型
    pub model: String,
    /// 额外的环境变量
    pub env_vars: std::collections::HashMap<String, String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent_type: AgentType::Codex,  // 默认使用 Codex 代理来调用 GLM
            cwd: std::env::current_dir().unwrap_or_default(),
            home_dir: dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".ai-agents"),
            model: "GLM-4.5".to_string(),  // 默认使用 GLM-4.5 模型
            env_vars: {
                let mut env = std::collections::HashMap::new();
                // 为 GLM 设置环境变量
                if let Ok(token) = std::env::var("GLM_AUTH_TOKEN") {
                    env.insert("GLM_AUTH_TOKEN".to_string(), token);
                }
                env
            },
        }
    }
}

/// 统一的 AI 代理管理器
pub struct AgentManager {
    agents: std::collections::HashMap<AgentType, Arc<dyn Agent>>,
    current_agent: Option<AgentType>,
    session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
}

impl AgentManager {
    /// 创建新的代理管理器
    pub fn new(
        session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
    ) -> Self {
        Self {
            agents: std::collections::HashMap::new(),
            current_agent: None,
            session_update_tx,
        }
    }

    /// 注册代理
    pub fn register_agent(
        &mut self,
        agent_type: AgentType,
        config: AgentConfig,
        client_tx: mpsc::UnboundedSender<AgentClientOp>,
    ) -> Result<(), Error> {
        let agent = match agent_type {
            AgentType::Claude => {
                let claude_config = claude::ClaudeConfig {
                    claude_home: config.home_dir,
                    cwd: config.cwd,
                    model: config.model,
                };
                
                let (client_tx_claude, _) = mpsc::unbounded_channel();
                claude::ClaudeAgentFactory::create_agent(
                    claude_config,
                    self.session_update_tx.clone(),
                    client_tx_claude,
                )
            }
            AgentType::Codex => {
                let codex_config = codex::CodexConfig {
                    cwd: config.cwd,
                    codex_home: config.home_dir,
                    model: config.model,
                };
                
                let (client_tx_codex, _) = mpsc::unbounded_channel();
                codex::CodexAgentFactory::create_agent(
                    codex_config,
                    self.session_update_tx.clone(),
                    client_tx_codex,
                )
            }
        };

        self.agents.insert(agent_type, agent);
        
        // 如果没有当前代理，设置为当前代理
        if self.current_agent.is_none() {
            self.current_agent = Some(agent_type);
        }

        info!("已注册 {} 代理", agent_type);
        Ok(())
    }

    /// 切换当前代理
    pub fn switch_agent(&mut self, agent_type: AgentType) -> Result<(), Error> {
        if !self.agents.contains_key(&agent_type) {
            return Err(Error::invalid_params()
                .with_data(format!("代理类型 {} 未注册", agent_type)));
        }

        self.current_agent = Some(agent_type);
        info!("切换到 {} 代理", agent_type);
        Ok(())
    }

    /// 获取当前代理
    pub fn current_agent(&self) -> Option<&Arc<dyn Agent>> {
        self.current_agent
            .and_then(|agent_type| self.agents.get(&agent_type))
    }

    /// 获取当前代理类型
    pub fn current_agent_type(&self) -> Option<AgentType> {
        self.current_agent
    }

    /// 获取所有可用的代理类型
    pub fn available_agents(&self) -> Vec<AgentType> {
        self.agents.keys().copied().collect()
    }

    /// 检查代理是否可用
    pub fn is_agent_available(agent_type: AgentType) -> bool {
        match agent_type {
            AgentType::Claude => claude::ClaudeAgentFactory::is_available(),
            AgentType::Codex => codex::CodexAgentFactory::is_available(),
        }
    }

    /// 获取代理支持的认证方法
    pub fn get_auth_methods(agent_type: AgentType) -> Vec<AuthMethod> {
        match agent_type {
            AgentType::Claude => claude::ClaudeAgentFactory::supported_auth_methods(),
            AgentType::Codex => codex::CodexAgentFactory::supported_auth_methods(),
        }
    }

    /// 自动检测并注册可用的代理
    pub fn auto_register_agents(
        &mut self,
        base_config: AgentConfig,
        client_tx: mpsc::UnboundedSender<AgentClientOp>,
    ) -> Result<Vec<AgentType>, Error> {
        let mut registered = Vec::new();

        for agent_type in [AgentType::Codex, AgentType::Claude] {  // 默认优先使用 Codex 支持 GLM
            if Self::is_agent_available(agent_type) {
                let mut config = base_config.clone();
                config.agent_type = agent_type;
                
                // 设置特定于代理的默认值
                match agent_type {
                    AgentType::Claude => {
                        config.home_dir = base_config.home_dir.join("claude");
                        if config.model == base_config.model {
                            config.model = "claude-3-5-sonnet-20241022".to_string();
                        }
                    }
                    AgentType::Codex => {
                        config.home_dir = base_config.home_dir.join("codex");
                        if config.model == base_config.model {
                            config.model = "GLM-4.5".to_string();  // Codex 默认使用 GLM-4.5
                        }
                        // 为 GLM 设置环境变量
                        if let Ok(token) = std::env::var("GLM_AUTH_TOKEN") {
                            config.env_vars.insert("GLM_AUTH_TOKEN".to_string(), token);
                        }
                    }
                }

                if self.register_agent(agent_type, config, client_tx.clone()).is_ok() {
                    registered.push(agent_type);
                }
            } else {
                warn!("{} 代理不可用，跳过注册", agent_type);
            }
        }

        if registered.is_empty() {
            return Err(Error::internal_error()
                .with_data("没有可用的 AI 代理"));
        }

        info!("自动注册了 {} 个代理: {:?}", registered.len(), registered);
        Ok(registered)
    }
}

/// 客户端操作枚举
#[derive(Debug)]
pub enum AgentClientOp {
    Claude(claude::ClientOp),
    Codex(codex::ClientOp),
}

/// 代理包装器，实现 Agent trait 来代理到当前选择的代理
pub struct ManagedAgent {
    manager: Arc<tokio::sync::RwLock<AgentManager>>,
}

impl ManagedAgent {
    pub fn new(manager: Arc<tokio::sync::RwLock<AgentManager>>) -> Self {
        Self { manager }
    }
}

#[async_trait::async_trait(?Send)]
impl Agent for ManagedAgent {
    async fn initialize(&self, args: InitializeRequest) -> Result<InitializeResponse, Error> {
        let manager = self.manager.read().await;
        if let Some(agent) = manager.current_agent() {
            agent.initialize(args).await
        } else {
            Err(Error::internal_error().with_data("没有可用的代理"))
        }
    }

    async fn authenticate(&self, args: AuthenticateRequest) -> Result<AuthenticateResponse, Error> {
        let manager = self.manager.read().await;
        if let Some(agent) = manager.current_agent() {
            agent.authenticate(args).await
        } else {
            Err(Error::internal_error().with_data("没有可用的代理"))
        }
    }

    async fn new_session(&self, args: NewSessionRequest) -> Result<NewSessionResponse, Error> {
        let manager = self.manager.read().await;
        if let Some(agent) = manager.current_agent() {
            agent.new_session(args).await
        } else {
            Err(Error::internal_error().with_data("没有可用的代理"))
        }
    }

    async fn load_session(&self, args: LoadSessionRequest) -> Result<LoadSessionResponse, Error> {
        let manager = self.manager.read().await;
        if let Some(agent) = manager.current_agent() {
            agent.load_session(args).await
        } else {
            Err(Error::internal_error().with_data("没有可用的代理"))
        }
    }

    async fn set_session_mode(&self, args: SetSessionModeRequest) -> Result<SetSessionModeResponse, Error> {
        let manager = self.manager.read().await;
        if let Some(agent) = manager.current_agent() {
            agent.set_session_mode(args).await
        } else {
            Err(Error::internal_error().with_data("没有可用的代理"))
        }
    }

    async fn prompt(&self, args: PromptRequest) -> Result<PromptResponse, Error> {
        let manager = self.manager.read().await;
        if let Some(agent) = manager.current_agent() {
            agent.prompt(args).await
        } else {
            Err(Error::internal_error().with_data("没有可用的代理"))
        }
    }

    async fn cancel(&self, args: CancelNotification) -> Result<(), Error> {
        let manager = self.manager.read().await;
        if let Some(agent) = manager.current_agent() {
            agent.cancel(args).await
        } else {
            Err(Error::internal_error().with_data("没有可用的代理"))
        }
    }

    async fn ext_method(&self, args: ExtRequest) -> Result<ExtResponse, Error> {
        let manager = self.manager.read().await;
        if let Some(agent) = manager.current_agent() {
            agent.ext_method(args).await
        } else {
            Err(Error::internal_error().with_data("没有可用的代理"))
        }
    }

    async fn ext_notification(&self, args: ExtNotification) -> Result<(), Error> {
        let manager = self.manager.read().await;
        if let Some(agent) = manager.current_agent() {
            agent.ext_notification(args).await
        } else {
            Err(Error::internal_error().with_data("没有可用的代理"))
        }
    }
}

/// 构建器模式来创建代理管理器
pub struct AgentManagerBuilder {
    config: AgentConfig,
    preferred_agents: Vec<AgentType>,
}

impl AgentManagerBuilder {
    pub fn new() -> Self {
        Self {
            config: AgentConfig::default(),
            preferred_agents: vec![AgentType::Codex, AgentType::Claude],  // 默认优先 Codex 支持 GLM
        }
    }

    pub fn with_config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_preferred_agents(mut self, agents: Vec<AgentType>) -> Self {
        self.preferred_agents = agents;
        self
    }

    pub fn build(
        self,
        session_update_tx: mpsc::UnboundedSender<(SessionNotification, Sender<()>)>,
        client_tx: mpsc::UnboundedSender<AgentClientOp>,
    ) -> Result<AgentManager, Error> {
        let mut manager = AgentManager::new(session_update_tx);
        
        // 尝试按偏好顺序注册代理
        for agent_type in self.preferred_agents {
            if AgentManager::is_agent_available(agent_type) {
                let mut config = self.config.clone();
                config.agent_type = agent_type;
                
                if manager.register_agent(agent_type, config, client_tx.clone()).is_ok() {
                    break; // 成功注册一个就停止
                }
            }
        }

        if manager.current_agent().is_none() {
            return Err(Error::internal_error()
                .with_data("无法注册任何 AI 代理"));
        }

        Ok(manager)
    }
}

impl Default for AgentManagerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_type_from_str() {
        assert_eq!("claude".parse::<AgentType>().unwrap(), AgentType::Claude);
        assert_eq!("codex".parse::<AgentType>().unwrap(), AgentType::Codex);
        assert!("unknown".parse::<AgentType>().is_err());
    }

    #[test]
    fn test_agent_config_default() {
        let config = AgentConfig::default();
        assert_eq!(config.agent_type, AgentType::Codex);  // 默认使用 Codex
        assert_eq!(config.model, "GLM-4.5");  // 默认使用 GLM-4.5 模型
    }

    #[tokio::test]
    async fn test_agent_manager_creation() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let manager = AgentManager::new(tx);
        assert!(manager.current_agent().is_none());
        assert!(manager.available_agents().is_empty());
    }
}