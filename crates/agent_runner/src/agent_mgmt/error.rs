//! Agent Management 错误类型
//!
//! 把各类失败(io/zip/npm/grpc)统一收敛为业务错误码,便于上层转换。

use shared_types::error_codes as ec;
use thiserror::Error;

pub type AgentMgmtResult<T> = Result<T, AgentMgmtError>;

#[derive(Debug, Error)]
pub enum AgentMgmtError {
    #[error("agent not found: {0}")]
    NotFound(String),

    #[error("invalid manifest: {0}")]
    InvalidManifest(String),

    #[error("checksum mismatch (expected {expected}, got {actual})")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("archive too large ({size} bytes, max {max})")]
    ArchiveBomb { size: u64, max: u64 },

    #[error("path traversal detected: {0}")]
    PathTraversal(String),

    #[error("command timeout: {0}")]
    CommandTimeout(String),

    #[error("install failed: {0}")]
    InstallFailed(String),

    #[error("binary too large ({size} bytes, max {max})")]
    BinaryTooLarge { size: u64, max: u64 },

    #[error("builtin agent is protected from uninstall")]
    BuiltinProtected,

    #[error("upload stream truncated: expected more data")]
    StreamTruncated,

    #[error("invalid upload chunk: {0}")]
    InvalidChunk(String),

    #[error("platform not found: {0}")]
    PlatformNotFound(String),

    #[error("invalid version: {0}")]
    InvalidVersion(String),

    #[error("install cancelled by force reinstall")]
    InstallCancelled,

    #[error("version already installed: {agent_id}@{version}")]
    VersionAlreadyInstalled { agent_id: String, version: String },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("archive: {0}")]
    Archive(String),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unsupported type: {0}")]
    UnsupportedType(String),
}

impl From<shared_types::version_util::VersionParseError> for AgentMgmtError {
    fn from(e: shared_types::version_util::VersionParseError) -> Self {
        Self::InvalidVersion(e.to_string())
    }
}

impl AgentMgmtError {
    /// 映射到业务错误码
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => ec::ERR_AGENT_MGMT_NOT_FOUND,
            Self::InvalidManifest(_) => ec::ERR_AGENT_MGMT_INVALID_MANIFEST,
            Self::ChecksumMismatch { .. } => ec::ERR_AGENT_MGMT_CHECKSUM_MISMATCH,
            Self::ArchiveBomb { .. } => ec::ERR_AGENT_MGMT_ARCHIVE_BOMB,
            Self::PathTraversal(_) => ec::ERR_AGENT_MGMT_PATH_TRAVERSAL,
            Self::CommandTimeout(_) => ec::ERR_AGENT_MGMT_COMMAND_TIMEOUT,
            Self::InstallFailed(_) => ec::ERR_AGENT_MGMT_INSTALL_FAILED,
            Self::BinaryTooLarge { .. } => ec::ERR_AGENT_MGMT_BINARY_TOO_LARGE,
            Self::BuiltinProtected => ec::ERR_AGENT_MGMT_BUILTIN_PROTECTED,
            Self::StreamTruncated => ec::ERR_AGENT_MGMT_STREAM_TRUNCATED,
            Self::InvalidChunk(_) => ec::ERR_AGENT_MGMT_INVALID_CHUNK,
            Self::PlatformNotFound(_) => ec::ERR_AGENT_MGMT_PLATFORM_NOT_FOUND,
            Self::InvalidVersion(_) => ec::ERR_AGENT_MGMT_INVALID_VERSION,
            Self::InstallCancelled => ec::ERR_AGENT_MGMT_INSTALL_CANCELLED,
            Self::VersionAlreadyInstalled { .. } => ec::ERR_AGENT_MGMT_ALREADY_INSTALLED,
            Self::UnsupportedType(_) => ec::ERR_AGENT_MGMT_UNSUPPORTED_TYPE,
            Self::Io(_) | Self::Archive(_) | Self::Json(_) => {
                ec::ERR_INTERNAL_SERVER_ERROR
            }
        }
    }
}
