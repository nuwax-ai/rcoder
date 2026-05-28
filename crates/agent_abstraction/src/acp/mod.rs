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
    #[error("Other error: {0}")]
    Other(String),
}

// Re-export shared types that are widely used across the codebase.
pub use shared_types::{CancelNotificationRequestWrapper, CancelResult};
