//! Agent servers configuration structure.
//!
//! 此模块负责配置的查询和管理。
//! 默认配置从 `default_agent_config` 模块加载（来源于 `configs/default_agents.json`）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use super::default_agent_config::{default_agent_servers, default_context_servers};
use crate::types::agent_config::AgentConfig;
use crate::types::error::{ConfigError, Result};
use crate::types::mcp_config::ContextServerConfig;
use crate::types::system_prompt::DEFAULT_SYSTEM_PROMPT;

/// Agent servers configuration
///
/// 包含所有 Agent 和 Context Server 的配置集合。
/// 默认配置来自 `configs/default_agents.json`（编译时嵌入）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentServersConfig {
    /// Agent servers configuration
    pub agent_servers: HashMap<String, AgentConfig>,

    /// Context servers configuration (MCP servers)
    #[serde(default)]
    pub context_servers: HashMap<String, ContextServerConfig>,
}

impl AgentServersConfig {
    /// 获取默认配置
    ///
    /// 返回从 `configs/default_agents.json` 加载的配置。
    /// 这是推荐的获取配置的方式。
    pub async fn load_or_default() -> Self {
        Self::default()
    }

    /// 从外部文件加载配置
    ///
    /// 用于加载用户自定义的配置文件，覆盖默认配置。
    pub async fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(ConfigError::file_not_found(path.display()).into());
        }

        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| ConfigError::Read(format!("{}: {}", path.display(), e)))?;

        Self::from_json(&content)
    }

    /// 从 JSON 字符串加载配置
    ///
    /// 用于解析用户传入的 JSON 配置。
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| ConfigError::Deserialization(e.to_string()).into())
    }

    /// Validate configuration
    pub fn validate(&self) -> Result<()> {
        // Validate agent servers
        for (name, config) in &self.agent_servers {
            if config.agent_id.is_empty() {
                return Err(
                    ConfigError::missing_field(format!("agent_servers.{}.agent_id", name)).into(),
                );
            }
            if config.command.is_empty() {
                return Err(
                    ConfigError::missing_field(format!("agent_servers.{}.command", name)).into(),
                );
            }
        }

        // Validate context servers
        for (name, config) in &self.context_servers {
            if config.command.is_none() {
                return Err(ConfigError::missing_field(format!(
                    "context_servers.{}.command",
                    name
                ))
                .into());
            }
        }

        Ok(())
    }

    /// Get all enabled agents
    pub fn get_enabled_agents(&self) -> Vec<&AgentConfig> {
        self.agent_servers
            .values()
            .filter(|config| config.enabled)
            .collect()
    }

    /// Get an agent configuration by ID
    pub fn get_agent(&self, agent_id: &str) -> Option<&AgentConfig> {
        self.agent_servers.get(agent_id)
    }

    /// 获取系统提示词（优先使用配置，否则使用默认值）
    ///
    /// # 参数
    /// - `agent_id`: Agent 标识符
    ///
    /// # 返回
    /// 根据配置的 source 字段决定返回内容：
    /// - `source: "embedded"` -> 返回编译时嵌入的默认提示词
    /// - `source: "custom"` 且 template 非空 -> 返回自定义模板内容
    /// - 其他情况 -> 回退到编译时嵌入的默认提示词
    ///
    /// 注意：此方法始终返回有效的系统提示词，不受 enabled 字段影响。
    /// 如需检查是否启用，请直接调用 `SystemPromptConfig::get_prompt()`。
    pub fn get_system_prompt(&self, agent_id: &str) -> String {
        if let Some(agent) = self.get_agent(agent_id) {
            // 如果有系统提示词配置，使用 get_prompt_or_default() 确保始终返回有效提示词
            if let Some(ref prompt_config) = agent.system_prompt {
                return prompt_config.get_prompt_or_default().to_string();
            }
        }
        // 回退到编译时嵌入的默认值
        DEFAULT_SYSTEM_PROMPT.to_string()
    }

    /// Get all enabled context servers
    pub fn get_enabled_context_servers(&self) -> Vec<&ContextServerConfig> {
        self.context_servers
            .values()
            .filter(|config| config.enabled)
            .collect()
    }

    /// Get a context server configuration by name
    pub fn get_context_server(&self, name: &str) -> Option<&ContextServerConfig> {
        self.context_servers.get(name)
    }

    /// 创建默认配置
    ///
    /// 从 `configs/default_agents.json` 加载的配置（编译时嵌入）。
    /// 修改 JSON 文件后重新编译即可生效。
    pub fn default() -> Self {
        Self {
            agent_servers: default_agent_servers(),
            context_servers: default_context_servers(),
        }
    }
}
