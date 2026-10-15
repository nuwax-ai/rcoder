//! Claude Code ACP Agent 启动器 (SACP 版本)
//!
//! Facade module for SACP launcher pieces. Public exports are kept compatible
//! with the historical `claude_code_sacp.rs` module.

mod config;
mod connection;
mod env;
mod launch_env;
mod launch_spawn;
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
pub use mcp::{convert_context_servers_sacp, set_mcp_proxy_log_dir};
pub use types::{SacpAgentLaunchConfig, SacpLauncherConnectionInfo};

// 豁免仅限测试模块：edition-2024 的 env 变异（set_var/remove_var）是 unsafe，
// 测试内有 ENV_TEST_LOCK 串行化保护（各处带 SAFETY 注释）
#[cfg(test)]
#[allow(unsafe_code)]
mod tests;
