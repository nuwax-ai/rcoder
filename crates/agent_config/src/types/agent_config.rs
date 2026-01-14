//! Agent configuration structure.

use serde::{Deserialize, Serialize};
use shared_types::ModelProviderConfig;
use std::collections::HashMap;

/// Agent configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Agent identifier
    pub agent_id: String,

    /// Command to execute
    pub command: String,

    /// Command arguments
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Installation configuration
    pub installation: super::installation::InstallationConfig,

    /// System prompt configuration
    #[serde(default)]
    pub system_prompt: Option<super::prompt_config::SystemPromptConfig>,

    /// User prompt configuration
    #[serde(default)]
    pub user_prompt: Option<super::prompt_config::UserPromptConfig>,

    /// Model provider configuration
    #[serde(default)]
    pub model_provider: Option<ModelProviderConfig>,

    /// Whether the agent is enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Metadata
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

/// Default enabled value
fn default_enabled() -> bool {
    true
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            agent_id: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: HashMap::new(),
            installation: super::installation::InstallationConfig::default(),
            system_prompt: None,
            user_prompt: None,
            model_provider: None,
            enabled: true,
            metadata: HashMap::new(),
        }
    }
}

impl AgentConfig {
    /// Create a new agent config
    pub fn new(agent_id: String) -> Self {
        Self {
            agent_id,
            ..Default::default()
        }
    }

    /// Get an environment variable
    pub fn get_env(&self, key: &str) -> Option<&String> {
        self.env.get(key)
    }

    /// Set an environment variable
    pub fn set_env(&mut self, key: String, value: String) {
        self.env.insert(key, value);
    }

    /// Check if agent is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Get model provider configuration
    pub fn get_model_provider(&self) -> Option<&ModelProviderConfig> {
        self.model_provider.as_ref()
    }

    /// Set model provider configuration
    pub fn set_model_provider(&mut self, provider: ModelProviderConfig) {
        self.model_provider = Some(provider);
    }

    /// Get the default model name if configured
    pub fn get_default_model(&self) -> Option<&str> {
        self.model_provider
            .as_ref()
            .map(|p| p.default_model.as_str())
    }

    /// Get the model provider name if configured
    pub fn get_model_provider_name(&self) -> Option<&str> {
        self.model_provider.as_ref().map(|p| p.name.as_str())
    }
}
