//! Agent abstraction layer error types

use thiserror::Error;

/// Agent abstraction layer error
#[derive(Error, Debug)]
pub enum AgentAbstractionError {
    #[error("connection error: {0}")]
    Connection(String),

    #[error("registry error: {0}")]
    Registry(String),

    #[error("process error: {0}")]
    Process(String),

    #[error("other error: {0}")]
    Other(String),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("type cast error")]
    Cast,

    #[error("not found: {0}")]
    NotFound(String),
}
