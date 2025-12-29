//! Agent Configuration Management
//!
//! This crate provides configuration management for agents in RCoder.

pub mod config;
pub mod installer;
pub mod types;

// Re-export main types
pub use config::prompt_assembler::PromptConfigAssembler;
pub use config::servers_config::AgentServersConfig;
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

// Default configuration exports
pub use config::default_agent_config::{
    default_agent_servers,
    default_agent_servers_for_service,
    default_context_servers,
    default_context_servers_for_service,
    get_default_agent,
    get_default_agent_for_service,
    get_default_config_by_service_type,
    get_default_context_server,
    get_default_context_server_for_service,
    CLAUDE_CODE_ACP_AGENT_ID,
};
