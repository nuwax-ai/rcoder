//! Agent lifecycle management module.

mod manager;

pub use manager::{AgentIdleStatus, AgentLifecycleManager, AgentStatusInfo};

/// Agent lifecycle error
#[derive(thiserror::Error, Debug)]
pub enum AgentLifecycleError {
    #[error("Agent not found: {0}")]
    NotFound(String),

    #[error("Agent already exists: {0}")]
    AlreadyExists(String),

    #[error("Process error: {0}")]
    Process(String),

    #[error("Other error: {0}")]
    Other(String),
}
