//! Error types for agent configuration.

use thiserror::Error;

/// Agent configuration error
#[derive(Error, Debug)]
pub enum AgentConfigError {
    /// Configuration error
    #[error("配置错误: {0}")]
    Configuration(#[from] ConfigError),

    /// I/O error
    #[error("I/O错误: {0}")]
    Io(#[from] IoError),

    /// Installation error
    #[error("安装错误: {0}")]
    Installation(#[from] InstallationError),

    /// Validation error
    #[error("验证错误: {0}")]
    Validation(#[from] ValidationError),

    /// Other error
    #[error("其他错误: {0}")]
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
    #[error("序列化错误: {0}")]
    Serialization(String),

    /// Deserialization error
    #[error("反序列化错误: {0}")]
    Deserialization(String),

    /// Missing required field
    #[error("缺少必需字段: {0}")]
    MissingField(String),

    /// Invalid value
    #[error("无效值: {0}")]
    InvalidValue(String),

    /// File not found
    #[error("文件未找到: {0}")]
    FileNotFound(String),

    /// Permission denied
    #[error("权限被拒绝: {0}")]
    PermissionDenied(String),

    /// Read error
    #[error("读取错误: {0}")]
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
    #[error("路径错误: {0:?}")]
    PathError(std::path::PathBuf),

    /// Read error
    #[error("读取错误: {0}")]
    Read(String),

    /// Write error
    #[error("写入错误: {0}")]
    Write(String),

    /// Create error
    #[error("创建错误: {0}")]
    Create(String),

    /// Delete error
    #[error("删除错误: {0}")]
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
    #[error("包未找到: {0}")]
    PackageNotFound(String),

    /// Installation failed
    #[error("Installation failed: {0}")]
    InstallationFailed(String),

    /// Validation failed
    #[error("验证失败: {0}")]
    ValidationFailed(String),

    /// Permission denied
    #[error("权限被拒绝: {0}")]
    PermissionDenied(String),
}

/// Validation error
#[derive(Error, Debug)]
pub enum ValidationError {
    /// Invalid configuration
    #[error("无效配置: {0}")]
    InvalidConfiguration(String),

    /// Missing dependency
    #[error("缺少依赖: {0}")]
    MissingDependency(String),

    /// Constraint violation
    #[error("约束违规: {0}")]
    ConstraintViolation(String),
}

// Convenience aliases
pub type Result<T> = std::result::Result<T, AgentConfigError>;
