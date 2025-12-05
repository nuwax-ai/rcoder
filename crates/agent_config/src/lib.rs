//! Agent Configuration Management
//!
//! This crate provides configuration management for agents in RCoder.

pub mod config;
pub mod installer;
pub mod resolver;
pub mod types;

// Re-export main types
pub use config::servers_config::AgentServersConfig;
pub use resolver::env_resolver::EnvironmentVariableResolver;
pub use types::agent_config::AgentConfig;
pub use types::agent_spec::AgentSpec;
pub use types::error::{AgentConfigError, ConfigError, Result};
pub use types::installation::{InstallationConfig, PackageManager};
pub use types::mcp_config::{ContextServerConfig, McpServerConfig, McpServerSource};
pub use types::prompt_config::{SystemPromptConfig, UserPromptConfig};
pub use types::system_prompt::DEFAULT_SYSTEM_PROMPT;
#[allow(deprecated)]
pub use types::system_prompt::PromptBuilder;

// Installer exports
pub use installer::{AgentInstallationManager, AgentInstaller, InstallResult, InstallationError, NpmInstaller};
