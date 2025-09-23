//! Claude AI Agent Library
//!
//! 提供基于 Claude Code 的 AI 代理服务，通过 ACP (Agent Client Protocol) 协议实现
//! 与 Claude Code CLI 工具的集成。

pub mod util;

pub use util::{
    ClaudeCodeAcpCommand, ClaudeCodeAcpConfig, ClaudeCodeAcpManager, ClaudeCodeAcpStatus,
    check_claude_acp_status, ensure_claude_acp_installed, get_claude_acp_command,
};

use agent_client_protocol::{Agent, AuthMethod, AuthMethodId, SessionNotification};
