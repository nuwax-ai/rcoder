//! Core type definitions for agent configuration.

pub mod agent_config;
pub mod agent_spec;
pub mod error;
pub mod installation;
pub mod mcp_config;
pub mod prompt_config;
pub mod system_prompt;

pub use agent_config::*;
pub use agent_spec::*;
pub use error::*;
pub use installation::*;
pub use mcp_config::*;
pub use prompt_config::*;
#[allow(deprecated)]
pub use system_prompt::PromptBuilder;
pub use system_prompt::DEFAULT_SYSTEM_PROMPT;
