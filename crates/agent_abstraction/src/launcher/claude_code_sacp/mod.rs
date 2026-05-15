//! Claude Code ACP Agent 启动器 (SACP 版本)
//!
//! Facade module for SACP launcher pieces. Public exports are kept compatible
//! with the historical `claude_code_sacp.rs` module.

mod config;
mod connection;
mod env;
mod launcher_impl;
mod mcp;
mod process;
mod types;

#[allow(unused_imports)]
pub use config::{
    get_default_sacp_agent_config, get_default_sacp_agent_config_with_resolver,
    load_sacp_agent_config, load_sacp_agent_config_with_resolver,
};
pub use launcher_impl::SacpClaudeCodeLauncher;
pub use mcp::convert_context_servers_sacp;
pub use types::{SacpAgentLaunchConfig, SacpLauncherConnectionInfo};

#[cfg(test)]
mod tests;
