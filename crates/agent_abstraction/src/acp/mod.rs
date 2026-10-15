//! ACP connection management module.
//!
//! This module re-exports shared types used across the ACP protocol layer.
//! The legacy `AgentConnection` struct has been removed — consumers now use
//! `SessionHandles` (from the `session` module) directly.

/// Placeholder error type
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("Connection error: {0}")]
    Connection(String),
    /// 等待 prompt 完成超时（R11：类型化——CLI 退出码分类不再匹配
    /// "timed out" 文案；仅表示等待完成超时，不含发送/连接超时）。
    #[error("prompt completion timed out after {timeout:?}")]
    Timeout { timeout: std::time::Duration },
    #[error("Other error: {0}")]
    Other(String),
}

// Re-export shared types that are widely used across the codebase.
pub use shared_types::{CancelNotificationRequestWrapper, CancelResult};
