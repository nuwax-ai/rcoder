//! Error types for agent configuration.

use thiserror::Error;

/// Agent configuration error
#[derive(Error, Debug)]
pub enum AgentConfigError {
    /// Configuration error
    #[error("configuration error: {0}")]
    Configuration(#[from] ConfigError),

    /// I/O error
    #[error("I/O error: {0}")]
    Io(#[from] IoError),

    /// Installation error
    #[error("installation error: {0}")]
    Installation(#[from] InstallationError),

    /// Validation error
    #[error("validation error: {0}")]
    Validation(#[from] ValidationError),

    /// Other error
    #[error("other error: {0}")]
    Other(String),
}

impl From<serde_json::Error> for AgentConfigError {
    fn from(e: serde_json::Error) -> Self {
        Self::Configuration(ConfigError::Serialization(e.to_string()))
    }
}

impl From<std::path::PathBuf> for AgentConfigError {
    fn from(path: std::path::PathBuf) -> Self {
        Self::Io(IoError::PathError(path))
    }
}

/// Configuration error
#[derive(Error, Debug)]
pub enum ConfigError {
    /// Serialization error
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Deserialization error
    #[error("deserialization error: {0}")]
    Deserialization(String),

    /// Missing required field
    #[error("missing required field: {0}")]
    MissingField(String),

    /// Invalid value
    #[error("invalid value: {0}")]
    InvalidValue(String),

    /// File not found
    #[error("file not found: {0}")]
    FileNotFound(String),

    /// Permission denied
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// Read error
    #[error("read error: {0}")]
    Read(String),
}

impl ConfigError {
    /// Create a missing field error
    pub fn missing_field(field: impl Into<String>) -> Self {
        Self::MissingField(field.into())
    }

    /// Create an invalid value error
    pub fn invalid_value(msg: impl Into<String>) -> Self {
        Self::InvalidValue(msg.into())
    }

    /// Create a file not found error
    pub fn file_not_found(path: impl std::fmt::Display) -> Self {
        Self::FileNotFound(path.to_string())
    }

    /// Create a read error
    pub fn read(path: impl std::fmt::Display, source: impl std::fmt::Display) -> Self {
        Self::Read(format!("{}: {}", path, source))
    }
}

/// I/O error
#[derive(Error, Debug)]
pub enum IoError {
    /// Path error
    #[error("path error: {0:?}")]
    PathError(std::path::PathBuf),

    /// Read error
    #[error("read error: {0}")]
    Read(String),

    /// Write error
    #[error("write error: {0}")]
    Write(String),

    /// Create error
    #[error("create error: {0}")]
    Create(String),

    /// Delete error
    #[error("delete error: {0}")]
    Delete(String),
}

impl IoError {
    /// Create a read error
    pub fn read(path: impl Into<String>, source: impl ToString) -> Self {
        Self::Read(format!("{}: {}", path.into(), source.to_string()))
    }

    /// Create a write error
    pub fn write(path: impl Into<String>, source: impl ToString) -> Self {
        Self::Write(format!("{}: {}", path.into(), source.to_string()))
    }
}

/// Installation error
#[derive(Error, Debug)]
pub enum InstallationError {
    /// Package not found
    #[error("package not found: {0}")]
    PackageNotFound(String),

    /// Installation failed
    #[error("installation failed: {0}")]
    InstallationFailed(String),

    /// Validation failed
    #[error("validation failed: {0}")]
    ValidationFailed(String),

    /// Permission denied
    #[error("permission denied: {0}")]
    PermissionDenied(String),
}

/// Validation error
#[derive(Error, Debug)]
pub enum ValidationError {
    /// Invalid configuration
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),

    /// Missing dependency
    #[error("missing dependency: {0}")]
    MissingDependency(String),

    /// Constraint violation
    #[error("constraint violation: {0}")]
    ConstraintViolation(String),
}

// Convenience aliases
pub type Result<T> = std::result::Result<T, AgentConfigError>;
