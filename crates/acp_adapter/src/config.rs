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

/// 认证方法
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuthenticationMethod {
    None,
    ApiKey {
        key: String,
        header_name: Option<String>,
    },
    OAuth {
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
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
        .with_api_key_auth(std::env::var("CLAUDE_API_KEY").unwrap_or_default())
        .with_mcp_enabled(true)
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
                if let Ok(key) = std::env::var("CLAUDE_API_KEY") {
                    config.authentication = AuthenticationMethod::ApiKey {
                        key,
                        header_name: Some("Authorization".to_string()),
                    };
                }
            }
            "codex" => {
                config.process.command = "codex".to_string();
                config.process.args = vec!["stdio".to_string()];
                if let Ok(key) = std::env::var("CODEX_API_KEY") {
                    config.authentication = AuthenticationMethod::ApiKey {
                        key,
                        header_name: Some("Authorization".to_string()),
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

    #[tokio::test]
    async fn test_config_validation_async() {
        let mut config = AcpConfig::default();
        assert!(config.validate().is_err());

        config.process.command = "test".to_string();
        assert!(config.validate().is_ok());
    }
}