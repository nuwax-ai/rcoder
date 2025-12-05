//! Agent specification structure.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Agent specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSpec {
    /// Agent identifier (matches the key in config file)
    pub agent_id: String,

    /// Command to execute
    pub command: String,

    /// Command arguments
    pub args: Vec<String>,

    /// Environment variables
    pub env: HashMap<String, String>,

    /// Installation configuration
    pub installation: super::installation::InstallationConfig,

    /// System prompt configuration
    pub system_prompt: Option<super::prompt_config::SystemPromptConfig>,

    /// User prompt configuration
    pub user_prompt: Option<super::prompt_config::UserPromptConfig>,

    /// Whether the agent is enabled
    pub enabled: bool,

    /// Metadata
    pub metadata: HashMap<String, String>,
}

impl AgentSpec {
    /// Create a new agent spec
    pub fn new(agent_id: String) -> Self {
        Self {
            agent_id,
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            installation: super::installation::InstallationConfig::default(),
            system_prompt: None,
            user_prompt: None,
            enabled: true,
            metadata: HashMap::new(),
        }
    }

    /// Convert to AgentConfig
    pub fn to_agent_config(&self) -> super::agent_config::AgentConfig {
        super::agent_config::AgentConfig {
            agent_id: self.agent_id.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            installation: self.installation.clone(),
            system_prompt: self.system_prompt.clone(),
            user_prompt: self.user_prompt.clone(),
            model_provider: None,
            enabled: self.enabled,
            metadata: self.metadata.clone(),
        }
    }
}
