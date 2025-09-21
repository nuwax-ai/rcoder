//! ACP 配置管理

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// 进程配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub working_dir: Option<PathBuf>,
    pub timeout_seconds: Option<u64>,
    pub restart_on_failure: bool,
    pub max_restarts: Option<u32>,
}

/// 连接配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub max_retries: u32,
    pub retry_delay_seconds: u64,
    pub timeout_seconds: u64,
    pub heartbeat_interval_seconds: u64,
    pub buffer_size: usize,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_delay_seconds: 1,
            timeout_seconds: 30,
            heartbeat_interval_seconds: 30,
            buffer_size: 1024,
        }
    }
}

/// 会话配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub max_history_messages: usize,
    pub timeout_seconds: u64,
    pub auto_save: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_history_messages: 100,
            timeout_seconds: 300,
            auto_save: true,
        }
    }
}

/// 模型提供商配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    /// 提供商名称 (如: glm, anthropic, openai, qwen, ernie, moonshot)
    pub name: String,
    /// API 基础 URL
    pub base_url: String,
    /// 环境变量中的密钥名称
    pub env_key: String,
    /// 是否需要 OpenAI 兼容的认证
    pub requires_openai_auth: bool,
    /// 默认模型名称
    pub default_model: String,
    /// 额外的配置参数
    pub extra_params: HashMap<String, String>,
}

impl ModelProviderConfig {
    /// 创建 GLM 提供商配置
    pub fn glm() -> Self {
        Self {
            name: "glm".to_string(),
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4".to_string(),
            env_key: "GLM_AUTH_TOKEN".to_string(),
            requires_openai_auth: false,
            default_model: "GLM-4.5".to_string(),
            extra_params: HashMap::new(),
        }
    }

    /// 创建 Claude 提供商配置
    pub fn claude() -> Self {
        Self {
            name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            env_key: "ANTHROPIC_API_KEY".to_string(),
            requires_openai_auth: false,
            default_model: "claude-3-5-sonnet-20241022".to_string(),
            extra_params: HashMap::new(),
        }
    }

    /// 创建 OpenAI 提供商配置
    pub fn openai() -> Self {
        Self {
            name: "openai".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            env_key: "OPENAI_API_KEY".to_string(),
            requires_openai_auth: true,
            default_model: "gpt-4o".to_string(),
            extra_params: HashMap::new(),
        }
    }

    /// 创建通义千问提供商配置
    pub fn qwen() -> Self {
        Self {
            name: "qwen".to_string(),
            base_url: "https://dashscope.aliyuncs.com/api/v1".to_string(),
            env_key: "QWEN_API_KEY".to_string(),
            requires_openai_auth: true,
            default_model: "qwen-max".to_string(),
            extra_params: HashMap::new(),
        }
    }

    /// 创建文心一言提供商配置
    pub fn ernie() -> Self {
        Self {
            name: "ernie".to_string(),
            base_url: "https://aip.baidubce.com/rpc/2.0/ai_custom/v1/wenxinworkshop".to_string(),
            env_key: "ERNIE_API_KEY".to_string(),
            requires_openai_auth: false,
            default_model: "ernie-4.0-8k".to_string(),
            extra_params: HashMap::new(),
        }
    }

    /// 创建月之暗面提供商配置（OpenAI 兼容）
    pub fn moonshot() -> Self {
        Self {
            name: "moonshot".to_string(),
            base_url: "https://api.moonshot.cn/v1".to_string(),
            env_key: "MOONSHOT_API_KEY".to_string(),
            requires_openai_auth: true,
            default_model: "moonshot-v1-8k".to_string(),
            extra_params: HashMap::new(),
        }
    }

    /// 创建 Kimi 提供商配置（Anthropic 兼容）
    pub fn kimi() -> Self {
        Self {
            name: "kimi".to_string(),
            base_url: "https://api.moonshot.ai/anthropic".to_string(),
            env_key: "ANTHROPIC_AUTH_TOKEN".to_string(),
            requires_openai_auth: false,
            default_model: "kimi-k2-0905".to_string(),
            extra_params: HashMap::new(),
        }
    }

    /// 创建自定义提供商配置
    pub fn custom(name: String, base_url: String, env_key: String, default_model: String) -> Self {
        Self {
            name,
            base_url,
            env_key,
            requires_openai_auth: false,
            default_model,
            extra_params: HashMap::new(),
        }
    }

    /// 获取环境变量中的认证令牌
    pub fn get_auth_token(&self) -> Option<String> {
        std::env::var(&self.env_key).ok()
    }

    /// 生成适用于代理的环境变量映射
    pub fn generate_env_vars(&self) -> HashMap<String, String> {
        let mut env_vars = HashMap::new();
        
        // 获取认证令牌
        if let Some(token) = self.get_auth_token() {
            if self.requires_openai_auth {
                // OpenAI 兼容的环境变量
                env_vars.insert("OPENAI_API_KEY".to_string(), token);
                env_vars.insert("OPENAI_BASE_URL".to_string(), self.base_url.clone());
            } else {
                // 自定义环境变量
                env_vars.insert(self.env_key.clone(), token);
                env_vars.insert(format!("{}_BASE_URL", self.name.to_uppercase()), self.base_url.clone());
            }
            
            // 添加额外的模型参数
            for (key, value) in &self.extra_params {
                env_vars.insert(key.clone(), value.clone());
            }
        }
        
        env_vars
    }
}

/// 认证方法
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthenticationMethod {
    None,
    ApiKey {
        key: String,
        header_name: Option<String>,
        base_url: Option<String>,
    },
    OAuth {
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
        base_url: Option<String>,
    },
    Custom {
        name: String,
        parameters: HashMap<String, String>,
    },
}

/// 环境配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    pub vars: HashMap<String, String>,
    pub working_dir: Option<PathBuf>,
    pub path_extensions: Vec<PathBuf>,
}

/// MCP 服务器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub working_dir: Option<PathBuf>,
    pub timeout_seconds: Option<u64>,
}

/// ACP 适配器主配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpConfig {
    /// 适配器名称
    pub name: String,

    /// 代理类型（如 "claude", "codex"）
    pub agent_type: String,

    /// 进程配置
    pub process: ProcessConfig,

    /// 连接配置
    pub connection: ConnectionConfig,

    /// 会话配置
    pub session: SessionConfig,

    /// 认证方法
    pub authentication: AuthenticationMethod,

    /// 环境配置
    pub environment: EnvironmentConfig,

    /// MCP 服务器配置
    pub mcp_servers: Vec<McpServerConfig>,

    /// 是否启用 MCP
    pub mcp_enabled: bool,

    /// 启用的工具列表
    pub enabled_tools: Vec<String>,

    /// 禁用的工具列表
    pub disabled_tools: Vec<String>,

    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,

    /// 使用的模型名称
    pub model_name: Option<String>,

    /// 自定义配置
    pub custom: serde_json::Value,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            agent_type: String::new(),
            process: ProcessConfig {
                command: String::new(),
                args: Vec::new(),
                env: HashMap::default(),
                working_dir: None,
                timeout_seconds: Some(60),
                restart_on_failure: true,
                max_restarts: Some(3),
            },
            connection: ConnectionConfig {
                max_retries: 3,
                retry_delay_seconds: 1,
                timeout_seconds: 30,
                heartbeat_interval_seconds: 30,
                buffer_size: 8192,
            },
            session: SessionConfig::default(),
            authentication: AuthenticationMethod::None,
            environment: EnvironmentConfig {
                vars: HashMap::default(),
                working_dir: None,
                path_extensions: Vec::new(),
            },
            mcp_servers: Vec::new(),
            mcp_enabled: false,
            enabled_tools: Vec::new(),
            disabled_tools: Vec::new(),
            model_provider: None,
            model_name: None,
            custom: serde_json::Value::Object(serde_json::Map::new()),
        }
    }
}

impl AcpConfig {
    /// 创建新的配置
    pub fn new(agent_type: String, command: String) -> Self {
        let name = agent_type.clone();
        Self {
            agent_type,
            name,
            process: ProcessConfig {
                command,
                args: Vec::new(),
                env: HashMap::default(),
                working_dir: None,
                timeout_seconds: Some(60),
                restart_on_failure: true,
                max_restarts: Some(3),
            },
            ..Default::default()
        }
    }

    /// 设置进程命令
    pub fn with_command(mut self, command: String) -> Self {
        self.process.command = command;
        self
    }

    /// 设置进程参数
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.process.args = args;
        self
    }

    /// 设置工作目录
    pub fn with_working_dir(mut self, dir: PathBuf) -> Self {
        self.process.working_dir = Some(dir.clone());
        self.environment.working_dir = Some(dir);
        self
    }

    /// 设置环境变量
    pub fn with_env(mut self, key: String, value: String) -> Self {
        self.process.env.insert(key.clone(), value.clone());
        self.environment.vars.insert(key, value);
        self
    }

    /// 设置 API 密钥认证
    pub fn with_api_key_auth(mut self, key: String) -> Self {
        self.authentication = AuthenticationMethod::ApiKey {
            key,
            header_name: Some("Authorization".to_string()),
            base_url: None,
        };
        self
    }

    /// 设置 API 密钥认证（带自定义 base_url）
    pub fn with_api_key_auth_and_url(mut self, key: String, base_url: String) -> Self {
        self.authentication = AuthenticationMethod::ApiKey {
            key,
            header_name: Some("Authorization".to_string()),
            base_url: Some(base_url),
        };
        self
    }

    /// 设置 API 密钥认证（完全自定义）
    pub fn with_custom_api_key_auth(
        mut self, 
        key: String, 
        header_name: Option<String>, 
        base_url: Option<String>
    ) -> Self {
        self.authentication = AuthenticationMethod::ApiKey {
            key,
            header_name,
            base_url,
        };
        self
    }

    /// 启用 MCP
    pub fn with_mcp_enabled(mut self, enabled: bool) -> Self {
        self.mcp_enabled = enabled;
        self
    }

    /// 添加 MCP 服务器
    pub fn with_mcp_server(mut self, server: McpServerConfig) -> Self {
        self.mcp_servers.push(server);
        self.mcp_enabled = true;
        self
    }

    /// 启用工具
    pub fn with_enabled_tool(mut self, tool: String) -> Self {
        self.enabled_tools.push(tool);
        self
    }

    /// 禁用工具
    pub fn with_disabled_tool(mut self, tool: String) -> Self {
        self.disabled_tools.push(tool);
        self
    }

    /// 设置超时时间
    pub fn with_timeout(mut self, timeout_seconds: u64) -> Self {
        self.process.timeout_seconds = Some(timeout_seconds);
        self.connection.timeout_seconds = timeout_seconds;
        self
    }

    /// 验证配置
    pub fn validate(&self) -> Result<(), String> {
        if self.process.command.is_empty() {
            return Err("进程命令不能为空".to_string());
        }

        if self.connection.timeout_seconds == 0 {
            return Err("连接超时时间必须大于 0".to_string());
        }

        if let Some(timeout) = self.process.timeout_seconds {
            if timeout == 0 {
                return Err("进程超时时间必须大于 0".to_string());
            }
        }

        if self.session.max_history_messages == 0 {
            return Err("最大历史消息数必须大于 0".to_string());
        }

        // 验证 MCP 服务器配置
        for server in &self.mcp_servers {
            if server.name.is_empty() {
                return Err("MCP 服务器名称不能为空".to_string());
            }
            if server.command.is_empty() {
                return Err(format!("MCP 服务器 '{}' 的命令不能为空", server.name));
            }
        }

        Ok(())
    }

    /// 获取完整的环境变量
    pub fn full_environment(&self) -> HashMap<String, String> {
        let mut env = std::env::vars().collect::<HashMap<_, _>>();

        // 添加配置中的环境变量
        env.extend(self.environment.vars.clone());
        env.extend(self.process.env.clone());

        // 添加 PATH 扩展
        if !self.environment.path_extensions.is_empty() {
            let current_path = env.get("PATH").cloned().unwrap_or_default();
            let new_paths = self.environment.path_extensions
                .iter()
                .filter_map(|p| p.to_str())
                .collect::<Vec<_>>()
                .join(":");

            env.insert("PATH".to_string(), format!("{}:{}", new_paths, current_path));
        }

        env
    }

    /// 合并自定义配置
    pub fn merge_custom(&mut self, custom: serde_json::Value) -> Result<(), String> {
        if let Some(custom_obj) = custom.as_object() {
            if let Some(self_obj) = self.custom.as_object_mut() {
                self_obj.extend(custom_obj.clone());
            } else {
                self.custom = custom;
            }
        } else {
            return Err("自定义配置必须是对象类型".to_string());
        }
        Ok(())
    }

    /// 从文件加载配置
    pub async fn from_file(path: &PathBuf) -> Result<Self, String> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| format!("读取配置文件失败: {}", e))?;

        let config: Self = serde_json::from_str(&content)
            .map_err(|e| format!("解析配置文件失败: {}", e))?;

        config.validate()?;
        Ok(config)
    }

    /// 保存配置到文件
    pub async fn to_file(&self, path: &PathBuf) -> Result<(), String> {
        self.validate()?;

        let content = serde_json::to_string_pretty(self)
            .map_err(|e| format!("序列化配置失败: {}", e))?;

        tokio::fs::write(path, content)
            .await
            .map_err(|e| format!("写入配置文件失败: {}", e))?;

        Ok(())
    }

    /// 创建 Claude Code 配置
    pub fn claude_code() -> Self {
        Self::new(
            "claude".to_string(),
            "npx".to_string(),
        )
        .with_args(vec![
            "@zed-industries/claude-code-acp".to_string()
        ])
        .with_mcp_enabled(true)
    }

    /// 创建 Claude Code 使用 Kimi 的配置
    pub fn claude_code_with_kimi() -> Self {
        Self::new(
            "claude".to_string(),
            "npx".to_string(),
        )
        .with_args(vec![
            "@zed-industries/claude-code-acp".to_string()
        ])
        .with_mcp_enabled(true)
        .with_model_provider(ModelProviderConfig::kimi())
        .with_model_name("kimi-k2-0905".to_string())
    }

    /// 创建 Claude Code 使用国内大模型的配置
    pub fn claude_code_with_domestic_model(provider: ModelProviderConfig, model_name: String) -> Self {
        Self::new(
            "claude".to_string(),
            "npx".to_string(),
        )
        .with_args(vec![
            "@zed-industries/claude-code-acp".to_string()
        ])
        .with_mcp_enabled(true)
        .with_model_provider(provider)
        .with_model_name(model_name)
    }

    /// 创建 Codex 配置
    pub fn codex() -> Self {
        Self::new(
            "codex".to_string(),
            "codex".to_string(),
        )
        .with_args(vec!["stdio".to_string()])
        .with_api_key_auth(std::env::var("CODEX_API_KEY").unwrap_or_default())
    }

    /// 从环境变量创建配置
    pub fn from_env(agent_type: String) -> Result<Self, String> {
        let mut config = Self::new(agent_type.clone(), String::new());

        // 根据代理类型设置默认配置
        match agent_type.as_str() {
            "claude" => {
                config.process.command = "npx".to_string();
                config.process.args = vec!["@zed-industries/claude-code-acp".to_string()];
                // 不需要设置 API Key，因为 claude-code-acp 会调用本地 claude 命令
                // 本地 claude 命令已经处理了认证
            }
            "codex" => {
                config.process.command = "codex".to_string();
                config.process.args = vec!["stdio".to_string()];
                if let Ok(key) = std::env::var("CODEX_API_KEY") {
                    config.authentication = AuthenticationMethod::ApiKey {
                        key,
                        header_name: Some("Authorization".to_string()),
                        base_url: None,
                    };
                }
            }
            _ => {
                return Err(format!("不支持的代理类型: {}", agent_type));
            }
        }

        // 从环境变量读取配置
        if let Ok(timeout) = std::env::var("ACP_TIMEOUT") {
            if let Ok(seconds) = timeout.parse::<u64>() {
                config = config.with_timeout(seconds);
            }
        }

        if let Ok(mcp_enabled) = std::env::var("ACP_MCP_ENABLED") {
            config.mcp_enabled = mcp_enabled.parse().unwrap_or(false);
        }

        if let Ok(work_dir) = std::env::var("ACP_WORKING_DIR") {
            config = config.with_working_dir(PathBuf::from(work_dir));
        }

        config.validate()?;
        Ok(config)
    }

    /// 创建国内大模型配置
    pub fn domestic_model(agent_type: String, command: String, api_key: String, base_url: String) -> Self {
        Self::new(agent_type, command)
            .with_custom_api_key_auth(api_key, Some("Authorization".to_string()), Some(base_url))
    }

    /// 从环境变量创建国内大模型配置
    pub fn domestic_model_from_env(agent_type: String, command: String) -> Result<Self, String> {
        let api_key = std::env::var("API_KEY")
            .or_else(|_| std::env::var("DOMESTIC_API_KEY"))
            .map_err(|_| "请设置 API_KEY 或 DOMESTIC_API_KEY 环境变量")?;
        
        let base_url = std::env::var("BASE_URL")
            .or_else(|_| std::env::var("DOMESTIC_BASE_URL"))
            .map_err(|_| "请设置 BASE_URL 或 DOMESTIC_BASE_URL 环境变量")?;

        Ok(Self::domestic_model(agent_type, command, api_key, base_url))
    }

    /// 设置模型提供商
    pub fn with_model_provider(mut self, provider: ModelProviderConfig) -> Self {
        self.model_provider = Some(provider);
        self
    }

    /// 设置模型名称
    pub fn with_model_name(mut self, model_name: String) -> Self {
        self.model_name = Some(model_name);
        self
    }

    /// 创建 GLM 配置
    pub fn glm(agent_type: String, command: String) -> Self {
        Self::new(agent_type, command)
            .with_model_provider(ModelProviderConfig::glm())
            .with_model_name(ModelProviderConfig::glm().default_model)
    }

    /// 创建 Claude 配置
    pub fn claude_provider(agent_type: String, command: String) -> Self {
        Self::new(agent_type, command)
            .with_model_provider(ModelProviderConfig::claude())
            .with_model_name(ModelProviderConfig::claude().default_model)
    }

    /// 创建 OpenAI 配置
    pub fn openai(agent_type: String, command: String) -> Self {
        Self::new(agent_type, command)
            .with_model_provider(ModelProviderConfig::openai())
            .with_model_name(ModelProviderConfig::openai().default_model)
    }

    /// 创建通义千问配置
    pub fn qwen(agent_type: String, command: String) -> Self {
        Self::new(agent_type, command)
            .with_model_provider(ModelProviderConfig::qwen())
            .with_model_name(ModelProviderConfig::qwen().default_model)
    }

    /// 创建文心一言配置
    pub fn ernie(agent_type: String, command: String) -> Self {
        Self::new(agent_type, command)
            .with_model_provider(ModelProviderConfig::ernie())
            .with_model_name(ModelProviderConfig::ernie().default_model)
    }

    /// 创建月之暗面配置
    pub fn moonshot(agent_type: String, command: String) -> Self {
        Self::new(agent_type, command)
            .with_model_provider(ModelProviderConfig::moonshot())
            .with_model_name(ModelProviderConfig::moonshot().default_model)
    }

    /// 创建自定义模型配置
    pub fn custom_model(
        agent_type: String, 
        command: String, 
        provider_name: String, 
        base_url: String, 
        env_key: String, 
        model_name: String
    ) -> Self {
        let provider = ModelProviderConfig::custom(provider_name, base_url, env_key, model_name.clone());
        Self::new(agent_type, command)
            .with_model_provider(provider)
            .with_model_name(model_name)
    }

    /// 获取完整的环境变量（包含模型提供商的环境变量）
    pub fn full_environment_with_provider(&self) -> HashMap<String, String> {
        let mut env = self.full_environment();

        // 如果配置了模型提供商，添加其环境变量
        if let Some(ref provider) = self.model_provider {
            let provider_env = provider.generate_env_vars();
            env.extend(provider_env);
        }

        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_creation() {
        let config = AcpConfig::new("test".to_string(), "test-command".to_string());
        assert_eq!(config.agent_type, "test");
        assert_eq!(config.process.command, "test-command");
    }

    #[test]
    fn test_config_validation() {
        let mut config = AcpConfig::default();
        assert!(config.validate().is_err());

        config.process.command = "test".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_claude_code_config() {
        let config = AcpConfig::claude_code();
        assert_eq!(config.agent_type, "claude");
        assert_eq!(config.process.command, "npx");
        assert_eq!(config.process.args, vec!["@zed-industries/claude-code-acp"]);
        assert!(config.mcp_enabled);
    }

    #[test]
    fn test_domestic_model_config() {
        let config = AcpConfig::domestic_model(
            "claude".to_string(),
            "python".to_string(),
            "test_key".to_string(),
            "https://api.example.com".to_string(),
        );
        assert_eq!(config.agent_type, "claude");
        assert_eq!(config.process.command, "python");
        match config.authentication {
            AuthenticationMethod::ApiKey { key, base_url, .. } => {
                assert_eq!(key, "test_key");
                assert_eq!(base_url, Some("https://api.example.com".to_string()));
            }
            _ => panic!("Expected ApiKey authentication"),
        }
    }

    #[test]
    fn test_custom_api_key_auth() {
        let config = AcpConfig::new("test".to_string(), "test-command".to_string())
            .with_custom_api_key_auth(
                "custom_key".to_string(),
                Some("X-API-Key".to_string()),
                Some("https://custom.api.com".to_string()),
            );
        
        match config.authentication {
            AuthenticationMethod::ApiKey { key, header_name, base_url } => {
                assert_eq!(key, "custom_key");
                assert_eq!(header_name, Some("X-API-Key".to_string()));
                assert_eq!(base_url, Some("https://custom.api.com".to_string()));
            }
            _ => panic!("Expected ApiKey authentication"),
        }
    }

    #[tokio::test]
    async fn test_config_validation_async() {
        let mut config = AcpConfig::default();
        assert!(config.validate().is_err());

        config.process.command = "test".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_model_provider_config() {
        let provider = ModelProviderConfig::glm();
        assert_eq!(provider.name, "glm");
        assert_eq!(provider.base_url, "https://open.bigmodel.cn/api/coding/paas/v4");
        assert_eq!(provider.env_key, "GLM_AUTH_TOKEN");
        assert_eq!(provider.default_model, "GLM-4.5");
        assert!(!provider.requires_openai_auth);
    }

    #[test]
    fn test_custom_model_config() {
        let config = AcpConfig::custom_model(
            "claude".to_string(),
            "python".to_string(),
            "custom_provider".to_string(),
            "https://api.custom.com".to_string(),
            "CUSTOM_API_KEY".to_string(),
            "custom-model".to_string(),
        );
        
        assert_eq!(config.agent_type, "claude");
        assert_eq!(config.process.command, "python");
        assert!(config.model_provider.is_some());
        assert_eq!(config.model_name, Some("custom-model".to_string()));
        
        let provider = config.model_provider.unwrap();
        assert_eq!(provider.name, "custom_provider");
        assert_eq!(provider.base_url, "https://api.custom.com");
        assert_eq!(provider.env_key, "CUSTOM_API_KEY");
    }

    #[test]
    fn test_glm_config() {
        let config = AcpConfig::glm("claude".to_string(), "python".to_string());
        assert_eq!(config.agent_type, "claude");
        assert_eq!(config.process.command, "python");
        assert!(config.model_provider.is_some());
        assert_eq!(config.model_name, Some("GLM-4.5".to_string()));
    }

    #[test]
    fn test_qwen_config() {
        let config = AcpConfig::qwen("claude".to_string(), "python".to_string());
        assert_eq!(config.agent_type, "claude");
        assert_eq!(config.process.command, "python");
        assert!(config.model_provider.is_some());
        assert_eq!(config.model_name, Some("qwen-max".to_string()));
    }
}