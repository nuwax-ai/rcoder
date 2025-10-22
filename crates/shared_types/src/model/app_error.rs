use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("serde_json::Error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    #[error("anyhow::Error: {0}")]
    AnyhowError(#[from] anyhow::Error),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Generic error: {0}")]
    Generic(String),
}

impl AppError {
    /// Create a generic error from a string
    pub fn generic(msg: impl Into<String>) -> Self {
        AppError::Generic(msg.into())
    }
}