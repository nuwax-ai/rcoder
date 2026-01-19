//! Chat 接口专用的 Agent 配置结构体
//!
//! 简化版本，只包含运行时配置，不包含提示词配置。
//! 提示词由独立的 system_prompt 和 user_prompt 入参控制。

use crate::service_config::ServiceResourceLimits;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;

/// Chat 接口的 Agent 配置
///
/// 包含单个 Agent 的运行时配置和多个 MCP 服务器配置。
/// 提示词由独立入参 (system_prompt, user_prompt) 控制，不在此结构中。
#[derive(Debug, Clone, Serialize, Deserialize, Default, ToSchema)]
pub struct ChatAgentConfig {
    /// 单个 Agent 服务器配置（可选）
    ///
    /// 用于覆盖默认的 Agent 执行命令、参数、环境变量等。
    /// 如果不传，使用内部默认配置 (claude-code-acp)。
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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[derive(Default)]
pub struct ChatAgentServerConfig {
    /// Agent 标识符（可选，默认使用 "claude-code-acp"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,

    /// 执行命令（如 "claude-code-acp", "custom-agent"）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// 命令参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,

    /// 环境变量
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,

    /// 元数据（可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
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
    /// 获取 Agent ID，默认返回 "claude-code-acp"
    pub fn get_agent_id(&self) -> &str {
        self.agent_id.as_deref().unwrap_or("claude-code-acp")
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
                "agent_id": "claude-code-acp",
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
        assert_eq!(
            config.agent_server.as_ref().unwrap().get_agent_id(),
            "claude-code-acp"
        );
    }

    #[test]
    fn test_get_agent_id_default() {
        let config = ChatAgentServerConfig::default();
        assert_eq!(config.get_agent_id(), "claude-code-acp");
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
