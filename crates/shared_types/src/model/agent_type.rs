//! Agent 类型定义 - rcoder 和 agent_runer 共用

use serde::{Deserialize, Serialize};
use super::model_provider::ModelProviderConfig;

/// 使用Agent代理的工具类型,都是使用ACP协议包装过的agent代理
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentType {
    /// OpenAI Codex 代理
    Codex,
    /// Claude Code 代理
    Claude
}

impl Default for AgentType {
    fn default() -> Self {
        Self::Claude
    }
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentType::Codex => write!(f, "codex"),
            AgentType::Claude => write!(f, "claude"),
        }
    }
}

impl std::str::FromStr for AgentType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "codex" => Ok(AgentType::Codex),
            "claude" => Ok(AgentType::Claude),
            _ => Err(format!("Invalid agent type: {}. Must be one of: codex, claude", s)),
        }
    }
}

impl AgentType {
    /// 从模型提供商配置推断 Agent 类型
    pub fn from_model_provider(model_provider: Option<&ModelProviderConfig>) -> Self {
        match model_provider {
            Some(provider) => match provider.name.as_str() {
                "anthropic" => AgentType::Claude,
                "openai" => AgentType::Codex,
                _ => AgentType::Claude, // 默认使用 Claude
            },
            None => AgentType::Claude, // 默认使用 Claude
        }
    }
}