//! Chat 接口专用的 Agent 配置结构体
//!
//! 简化版本，只包含运行时配置，不包含提示词配置。
//! 提示词由独立的 system_prompt 和 user_prompt 入参控制。

use crate::service_config::ServiceResourceLimits;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;

/// Agent permission approval mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// Automatically allow safe permission requests.
    #[default]
    Yolo,
    /// Ask the user before resolving permission requests.
    Ask,
}

impl AgentMode {
    pub const DEFAULT_STR: &'static str = "yolo";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Yolo => "yolo",
            Self::Ask => "ask",
        }
    }

    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value
            .unwrap_or(Self::DEFAULT_STR)
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "yolo" => Ok(Self::Yolo),
            "ask" => Ok(Self::Ask),
            other => Err(format!("agent_mode must be yolo or ask, got: {other}")),
        }
    }
}

/// Chat 接口的 Agent 配置
///
/// 包含单个 Agent 的运行时配置和多个 MCP 服务器配置。
/// 提示词由独立入参 (system_prompt, user_prompt) 控制，不在此结构中。
#[derive(Debug, Clone, Serialize, Deserialize, Default, ToSchema)]
pub struct ChatAgentConfig {
    /// 单个 Agent 服务器配置（可选）
    ///
    /// 用于覆盖默认的 Agent 执行命令、参数、环境变量等。
    /// 如果不传，使用内部默认配置 (claude-code-acp-ts)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_server: Option<ChatAgentServerConfig>,

    /// MCP 服务器配置（Context Servers）
    ///
    /// 可配置多个 MCP 工具服务器。
    /// 如果不传，使用内部默认的 MCP 配置。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub context_servers: HashMap<String, ChatContextServerConfig>,

    /// 可选的容器资源限制配置
    ///
    /// 如果提供，将覆盖服务级别的默认资源限制。
    /// 支持动态设置内存、CPU、Swap、磁盘和进程数等资源限制。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_limits: Option<ServiceResourceLimits>,
}

/// 单个 Agent 服务器配置
///
/// 对应内部 AgentConfig 的简化版本，只暴露必要的运行时配置。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct ChatAgentServerConfig {
    /// Agent 标识符（可选，默认使用 "claude-code-acp-ts"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,

    /// 执行命令（如 "claude-code-acp-ts", "custom-agent"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// 命令参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,

    /// 环境变量
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,

    /// 模型环境变量显式绑定规则
    ///
    /// 用于声明某个 Agent env key 应该绑定到 model_provider 的哪个字段。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_env_bindings: Vec<ModelEnvBinding>,

    /// Permission approval mode: "yolo" (default) or "ask".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_mode: Option<String>,

    /// 元数据（可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

/// 模型环境变量绑定规则
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ModelEnvBinding {
    /// Agent 子进程环境变量名
    pub env_key: String,
    /// 绑定来源
    pub source: ModelEnvBindingSource,
}

/// 模型环境变量绑定来源
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelEnvBindingSource {
    ApiKey,
    BaseUrl,
    DefaultModel,
    ProviderName,
}

/// MCP 服务器配置
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ChatContextServerConfig {
    /// 服务器来源类型: "custom" 或 "local"
    #[serde(default = "default_custom")]
    pub source: String,

    /// 是否启用
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// 执行命令 (如 "bunx", "uvx", "npx")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// 命令参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,

    /// 环境变量
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

fn default_custom() -> String {
    "custom".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for ChatContextServerConfig {
    fn default() -> Self {
        Self {
            source: "custom".to_string(),
            enabled: true,
            command: None,
            args: None,
            env: None,
        }
    }
}

impl ChatAgentConfig {
    /// 检查是否有 Agent 服务器配置
    pub fn has_agent_server(&self) -> bool {
        self.agent_server.is_some()
    }

    /// 检查是否有 MCP 服务器配置
    pub fn has_context_servers(&self) -> bool {
        !self.context_servers.is_empty()
    }

    /// 获取启用的 MCP 服务器
    pub fn get_enabled_context_servers(&self) -> HashMap<String, &ChatContextServerConfig> {
        self.context_servers
            .iter()
            .filter(|(_, config)| config.enabled)
            .map(|(name, config)| (name.clone(), config))
            .collect()
    }
}

impl ChatAgentServerConfig {
    /// 获取 Agent ID，默认返回 "claude-code-acp-ts"
    pub fn get_agent_id(&self) -> &str {
        self.agent_id.as_deref().unwrap_or("claude-code-acp-ts")
    }

    /// Resolve and validate the permission approval mode.
    pub fn agent_mode(&self) -> Result<AgentMode, String> {
        AgentMode::parse(self.agent_mode.as_deref())
    }

    pub fn agent_mode_str(&self) -> Result<&'static str, String> {
        Ok(self.agent_mode()?.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_agent_config_default() {
        let config = ChatAgentConfig::default();
        assert!(config.agent_server.is_none());
        assert!(config.context_servers.is_empty());
        assert!(!config.has_agent_server());
        assert!(!config.has_context_servers());
    }

    #[test]
    fn test_chat_agent_config_json_serialize() {
        let config = ChatAgentConfig {
            agent_server: Some(ChatAgentServerConfig {
                agent_id: Some("test-agent".to_string()),
                command: Some("test-cmd".to_string()),
                ..Default::default()
            }),
            context_servers: HashMap::new(),
            resource_limits: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("test-agent"));
        assert!(json.contains("test-cmd"));
    }

    #[test]
    fn test_chat_agent_config_json_deserialize() {
        let json = r#"{
            "agent_server": {
                "agent_id": "claude-code-acp-ts",
                "env": {"RUST_LOG": "debug"}
            },
            "context_servers": {
                "context7": {
                    "source": "custom",
                    "enabled": true,
                    "command": "bunx",
                    "args": ["-y", "@upstash/context7-mcp"]
                }
            }
        }"#;
        let config: ChatAgentConfig = serde_json::from_str(json).unwrap();
        assert!(config.has_agent_server());
        assert!(config.has_context_servers());
        assert!(
            config
                .agent_server
                .as_ref()
                .unwrap()
                .model_env_bindings
                .is_empty()
        );
        assert_eq!(
            config.agent_server.as_ref().unwrap().get_agent_id(),
            "claude-code-acp-ts"
        );
    }

    #[test]
    fn test_chat_agent_server_model_env_bindings_json_deserialize() {
        let json = r#"{
            "agent_server": {
                "agent_id": "nuwax-codex-acp",
                "env": {"CODEX_MODEL": "placeholder"},
                "model_env_bindings": [
                    {"env_key": "CODEX_API_KEY", "source": "api_key"},
                    {"env_key": "CODEX_BASE_URL", "source": "base_url"},
                    {"env_key": "CODEX_MODEL", "source": "default_model"},
                    {"env_key": "CODEX_PROVIDER", "source": "provider_name"}
                ]
            }
        }"#;
        let config: ChatAgentConfig = serde_json::from_str(json).unwrap();
        let bindings = &config.agent_server.unwrap().model_env_bindings;

        assert_eq!(bindings.len(), 4);
        assert_eq!(bindings[0].env_key, "CODEX_API_KEY");
        assert_eq!(bindings[0].source, ModelEnvBindingSource::ApiKey);
        assert_eq!(bindings[1].source, ModelEnvBindingSource::BaseUrl);
        assert_eq!(bindings[2].source, ModelEnvBindingSource::DefaultModel);
        assert_eq!(bindings[3].source, ModelEnvBindingSource::ProviderName);
    }

    #[test]
    fn test_get_agent_id_default() {
        let config = ChatAgentServerConfig::default();
        assert_eq!(config.get_agent_id(), "claude-code-acp-ts");
    }

    #[test]
    fn test_agent_mode_default_and_validation() {
        let config = ChatAgentServerConfig::default();
        assert_eq!(config.agent_mode().unwrap(), AgentMode::Yolo);

        let config = ChatAgentServerConfig {
            agent_mode: Some("ask".to_string()),
            ..Default::default()
        };
        assert_eq!(config.agent_mode().unwrap(), AgentMode::Ask);

        let config = ChatAgentServerConfig {
            agent_mode: Some("YOLO".to_string()),
            ..Default::default()
        };
        assert_eq!(config.agent_mode().unwrap(), AgentMode::Yolo);

        let config = ChatAgentServerConfig {
            agent_mode: Some("careful".to_string()),
            ..Default::default()
        };
        assert!(config.agent_mode().is_err());
    }

    #[test]
    fn test_get_enabled_context_servers() {
        let mut context_servers = HashMap::new();
        context_servers.insert(
            "enabled".to_string(),
            ChatContextServerConfig {
                enabled: true,
                ..Default::default()
            },
        );
        context_servers.insert(
            "disabled".to_string(),
            ChatContextServerConfig {
                enabled: false,
                ..Default::default()
            },
        );
        let config = ChatAgentConfig {
            agent_server: None,
            context_servers,
            resource_limits: None,
        };
        let enabled = config.get_enabled_context_servers();
        assert_eq!(enabled.len(), 1);
        assert!(enabled.contains_key("enabled"));
    }
}
