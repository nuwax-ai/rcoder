//! Compatibility layer for existing agent implementations
//!
//! This module provides compatibility wrappers for migrating existing
//! agent implementations to the new abstraction layer.

mod channel_utils;
mod claude_code_agent;
mod claude_code_launcher;

pub use channel_utils::{spawn_cancel_handler_for_agent, spawn_prompt_handler_for_agent};
pub use claude_code_agent::{ClaudeCodeAcpAgent, ClaudeCodeAcpAgentConfig};
pub use claude_code_launcher::{
    convert_context_servers, get_default_agent_config, load_agent_config, AgentLaunchConfig,
    ClaudeCodeLauncher, LauncherConnectionInfo,
};
