//! Chat 接口专用的 Agent 配置结构体
//!
//! 简化版本，只包含运行时配置，不包含提示词配置。
//! 提示词由独立的 system_prompt 和 user_prompt 入参控制。

use crate::agent_mgmt_types::PlatformEntry;
use crate::service_config::ServiceResourceLimits;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
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

impl FromStr for AgentMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
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

    /// 自动重载配置（可选）
    ///
    /// 启用后，当检测到 Agent 二进制文件发生变化时，自动停止旧进程并启动新进程。
    /// 主要用于 DevComputer 调试场景，实现热重载开发体验。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_reload: Option<AutoReloadConfig>,
}

/// 自动重载配置
///
/// 控制 Agent 二进制文件变化检测和自动重启行为。
/// 主要用于 DevComputer 调试场景，开发者编译新 agent 后无需手动重启。
///
/// Auto-Reload 配置（简化版）
///
/// 当 `enabled=true` 时，每次请求都会重启 ACP agent 进程，
/// 并尝试通过 session_id 恢复历史上下文。
///
/// 适用场景：`/devcomputer/chat` 接口用于调试 ACP agent 功能，
/// 每次请求都应使用最新的 agent 代码。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AutoReloadConfig {
    /// 是否启用自动重载
    ///
    /// 启用后，每次请求都会重启 ACP agent 进程，
    /// 并尝试通过 session_id 恢复历史上下文。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for AutoReloadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
        }
    }
}

impl AutoReloadConfig {
    /// 创建一个默认启用的配置
    pub fn default_enabled() -> Self {
        Self::default()
    }

    /// 创建一个禁用的配置
    pub fn disabled() -> Self {
        Self {
            enabled: false,
        }
    }
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

    /// 工具审批规则（可选）
    ///
    /// 用于精细控制工具审批行为：
    /// - YOLO 模式下可配置特定工具需要审批
    /// - ASK 模式下可配置特定工具自动放行
    /// - 支持 deny 直接拒绝
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_approval_rules: Option<Vec<ToolApprovalRule>>,

    /// 元数据（可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,

    /// 期望安装的版本号（semver 格式，如 "1.2.0"）
    ///
    /// 与 `platforms` 配合使用：chat handler 在启动 agent 前自动检查版本是否已安装，
    /// 未安装则自动下载安装。版本归一化：v1.0.0 和 1.0.0 视为同一版本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// 多平台下载地址
    ///
    /// key 格式: `{os}-{arch}`，如 "linux-x86_64"、"linux-aarch64"。
    /// 与 `version` 配合使用：提供后 chat handler 自动检查/安装 agent。
    /// 要求 `agent_id` 和 `command` 必须有值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<HashMap<String, PlatformEntry>>,
}

/// 工具审批规则
///
/// 用于精细控制工具审批行为，规则独立于 agent_mode，按配置的 action 生效。
/// - `patterns` 使用 glob 通配符语法（大小写不敏感）
/// - `tool_kind` 决定 kind 过滤：Some(x) 仅匹配 kind=x；None 不过滤（覆盖所有类别）。匹配目标按实际工具 kind：Execute→命令内容，其他→工具名
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ToolApprovalRule {
    /// 通配符模式列表（大小写不敏感，任一命中即触发，OR 逻辑）
    pub patterns: Vec<String>,
    /// 审批动作: "ask" | "allow" | "deny"
    pub action: ToolApprovalAction,
    /// ACP ToolKind 过滤（可选），None 表示不按 kind 过滤（匹配所有类别）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_kind: Option<String>,
}

/// 审批动作枚举
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalAction {
    /// 要求用户审批（即使在 YOLO 模式下）
    Ask,
    /// 自动放行（即使在 ASK 模式下）
    Allow,
    /// 直接拒绝，不询问用户
    Deny,
}

impl ToolApprovalAction {
    pub const VALID_VALUES: &'static [&'static str] = &["ask", "allow", "deny"];

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ask" => Ok(Self::Ask),
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            other => Err(format!(
                "action must be one of {:?}, got: {}",
                Self::VALID_VALUES,
                other
            )),
        }
    }
}

/// ACP ToolKind 合法值
pub const VALID_TOOL_KINDS: &[&str] = &[
    "Read",
    "Edit",
    "Delete",
    "Move",
    "Search",
    "Execute",
    "Think",
    "Fetch",
    "SwitchMode",
    "Other",
];

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
    /// 获取 Agent ID，默认返回内置 agent ID
    pub fn get_agent_id(&self) -> &str {
        self.agent_id.as_deref().unwrap_or(crate::DEFAULT_AGENT_ID)
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
            auto_reload: None,
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
            auto_reload: None,
        };
        let enabled = config.get_enabled_context_servers();
        assert_eq!(enabled.len(), 1);
        assert!(enabled.contains_key("enabled"));
    }
}
