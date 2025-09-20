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

/// 模型提供商配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    /// 提供商名称 (如: glm, anthropic, openai)
    pub name: String,
    /// API 基础 URL
    pub base_url: String,
    /// 环境变量中的密钥名称
    pub env_key: String,
    /// 是否需要 OpenAI 兼容的认证
    pub requires_openai_auth: bool,
    /// 额外的配置参数
    pub extra_params: std::collections::HashMap<String, String>,
}

impl ModelProviderConfig {
    /// 创建 GLM 提供商配置
    pub fn glm() -> Self {
        Self {
            name: "glm".to_string(),
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4".to_string(),
            env_key: "GLM_AUTH_TOKEN".to_string(),
            requires_openai_auth: false,
            extra_params: std::collections::HashMap::new(),
        }
    }

    /// 创建 Claude 提供商配置
    pub fn claude() -> Self {
        Self {
            name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            env_key: "ANTHROPIC_API_KEY".to_string(),
            requires_openai_auth: false,
            extra_params: std::collections::HashMap::new(),
        }
    }

    /// 创建 GLM 通过 Anthropic 接口的配置
    pub fn glm_anthropic() -> Self {
        let mut extra_params = std::collections::HashMap::new();
        extra_params.insert("ANTHROPIC_MODEL".to_string(), "GLM-4.5".to_string());
        extra_params.insert("ANTHROPIC_SMALL_FAST_MODEL".to_string(), "GLM-4.5-Air".to_string());
        
        Self {
            name: "anthropic_glm".to_string(),
            base_url: "https://open.bigmodel.cn/api/anthropic".to_string(),
            env_key: "GLM_AUTH_TOKEN".to_string(),
            requires_openai_auth: false,
            extra_params,
        }
    }

    /// 创建 OpenAI 提供商配置
    pub fn openai() -> Self {
        Self {
            name: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            env_key: "OPENAI_API_KEY".to_string(),
            requires_openai_auth: true,
            extra_params: std::collections::HashMap::new(),
        }
    }

    /// 获取环境变量中的认证令牌
    pub fn get_auth_token(&self) -> Option<String> {
        std::env::var(&self.env_key).ok()
    }

    /// 生成适用于代理的环境变量映射
    pub fn generate_env_vars(&self, agent_type: AgentType) -> std::collections::HashMap<String, String> {
        let mut env_vars = std::collections::HashMap::new();
        
        // 获取认证令牌
        if let Some(token) = self.get_auth_token() {
            match agent_type {
                AgentType::Codex => {
                    // 为 Codex 设置 OpenAI 兼容的环境变量
                    env_vars.insert("OPENAI_API_KEY".to_string(), token);
                    env_vars.insert("OPENAI_BASE_URL".to_string(), self.base_url.clone());
                }
                AgentType::Claude => {
                    // 为 Claude Code 设置 Anthropic 环境变量
                    env_vars.insert("ANTHROPIC_AUTH_TOKEN".to_string(), token);
                    env_vars.insert("ANTHROPIC_BASE_URL".to_string(), self.base_url.clone());
                    
                    // 添加额外的模型参数
                    for (key, value) in &self.extra_params {
                        env_vars.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        
        env_vars
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
    /// 模型提供商配置
    pub provider: ModelProviderConfig,
    /// 额外的环境变量
    pub env_vars: std::collections::HashMap<String, String>,
    /// 推理努力程度 (如: high, medium, low)
    pub reasoning_effort: String,
    /// 首选的认证方法
    pub preferred_auth_method: String,
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
            provider: ModelProviderConfig::glm(),  // 默认使用 GLM 提供商
            reasoning_effort: "high".to_string(),  // 默认高推理努力程度
            preferred_auth_method: "apikey".to_string(),  // 默认使用 API Key 认证
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
        // 生成与代理类型匹配的环境变量
        let env_vars = config.provider.generate_env_vars(agent_type);
        
        // 设置环境变量到当前进程中（代理启动子进程时会继承这些环境变量）
        for (key, value) in &env_vars {
            // 使用 unsafe 块调用 set_var
            unsafe {
                std::env::set_var(key, value);
            }
        }
        
        // 合并用户自定义的环境变量
        for (key, value) in &config.env_vars {
            unsafe {
                std::env::set_var(key, value);
            }
        }
        
        let agent = match agent_type {
            AgentType::Claude => {
                let claude_config = claude::ClaudeConfig {
                    claude_home: config.home_dir,
                    cwd: config.cwd,
                    model: config.model.clone(),
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
                    model: config.model.clone(),
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

        info!("已注册 {} 代理，使用模型: {}\n提供商: {}\nBase URL: {}\n设置环境变量: {:?}", 
              agent_type, config.model, config.provider.name, config.provider.base_url, env_vars.keys().collect::<Vec<_>>());
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
                    }
                    AgentType::Codex => {
                        config.home_dir = base_config.home_dir.join("codex");
                    }
                }

                if self.register_agent(agent_type, config, client_tx.clone()).is_ok() {
                    registered.push(agent_type);
                } else {
                    warn!("注册 {} 代理失败", agent_type);
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
        assert_eq!(config.provider.name, "glm");  // 默认使用 GLM 提供商
        assert_eq!(config.reasoning_effort, "high");
        assert_eq!(config.preferred_auth_method, "apikey");
    }

    #[test]
    fn test_model_provider_config_glm() {
        let provider = ModelProviderConfig::glm();
        assert_eq!(provider.name, "glm");
        assert_eq!(provider.base_url, "https://open.bigmodel.cn/api/coding/paas/v4");
        assert_eq!(provider.env_key, "GLM_AUTH_TOKEN");
        assert!(!provider.requires_openai_auth);
    }

    #[test]
    fn test_model_provider_config_claude() {
        let provider = ModelProviderConfig::claude();
        assert_eq!(provider.name, "anthropic");
        assert_eq!(provider.base_url, "https://api.anthropic.com");
        assert_eq!(provider.env_key, "ANTHROPIC_API_KEY");
        assert!(!provider.requires_openai_auth);
    }

    #[test]
    fn test_model_provider_config_glm_anthropic() {
        let provider = ModelProviderConfig::glm_anthropic();
        assert_eq!(provider.name, "anthropic_glm");
        assert_eq!(provider.base_url, "https://open.bigmodel.cn/api/anthropic");
        assert_eq!(provider.env_key, "GLM_AUTH_TOKEN");
        assert!(provider.extra_params.contains_key("ANTHROPIC_MODEL"));
    }

    #[test]
    fn test_generate_env_vars_codex() {
        let provider = ModelProviderConfig::glm();
        // 设置测试环境变量
        unsafe {
            std::env::set_var("GLM_AUTH_TOKEN", "test-token");
        }
        
        let env_vars = provider.generate_env_vars(AgentType::Codex);
        assert_eq!(env_vars.get("OPENAI_API_KEY").unwrap(), "test-token");
        assert_eq!(env_vars.get("OPENAI_BASE_URL").unwrap(), &provider.base_url);
        
        // 清理环境变量
        unsafe {
            std::env::remove_var("GLM_AUTH_TOKEN");
        }
    }

    #[tokio::test]
    async fn test_agent_manager_creation() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let manager = AgentManager::new(tx);
        assert!(manager.current_agent().is_none());
        assert!(manager.available_agents().is_empty());
    }
}