//! Claude AI Agent Library
//!
//! 提供基于 Claude Code 的 AI 代理服务，通过 ACP (Agent Client Protocol) 协议实现
//! 与 Claude Code CLI 工具的集成。

mod connection;
mod util;

pub use connection::{
    ClaudeCodeAcpClient, ClaudeCodeAcpConnection, ClaudeCodeAcpConnectionManager,
    ClaudeCodeAcpConnector,
};
pub use util::*;
