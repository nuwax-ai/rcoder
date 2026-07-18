//! pnpm 后端统一错误模型。

use thiserror::Error;

use super::types::InstallSummary;

/// 对外稳定的安装失败分类。pnpm 新错误码仍会回退到 `Unknown`，原始 code 会保留。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    RegistryAuth,
    PackageNotFound,
    NetworkTimeout,
    NetworkUnavailable,
    LockfileMismatch,
    UnsupportedEngine,
    LifecycleScript,
    DiskFull,
    PermissionDenied,
    StoreCorrupted,
    Unknown,
}

impl std::fmt::Display for FailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::RegistryAuth => "registry authentication failed",
            Self::PackageNotFound => "package or version not found",
            Self::NetworkTimeout => "registry request timed out",
            Self::NetworkUnavailable => "registry network unavailable",
            Self::LockfileMismatch => "lockfile does not match package manifest",
            Self::UnsupportedEngine => "unsupported Node.js or package engine",
            Self::LifecycleScript => "dependency lifecycle script failed",
            Self::DiskFull => "disk is full",
            Self::PermissionDenied => "filesystem permission denied",
            Self::StoreCorrupted => "pnpm store data is corrupted",
            Self::Unknown => "unknown pnpm failure",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("failed to start pnpm: {source}")]
    Spawn {
        #[source]
        source: std::io::Error,
    },
    #[error("failed while waiting for pnpm: {source}")]
    Wait {
        #[source]
        source: std::io::Error,
    },
    #[error("pnpm install timed out after {timeout_secs}s")]
    TimedOut { timeout_secs: u64 },
    #[error("pnpm install failed (exit {exit_code}, {kind}{code_suffix}): {message}")]
    Failed {
        exit_code: i32,
        kind: FailureKind,
        code: Option<String>,
        code_suffix: String,
        message: String,
        summary: Box<InstallSummary>,
    },
}

impl InstallError {
    pub fn kind(&self) -> Option<FailureKind> {
        match self {
            Self::Failed { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Failed { code, .. } => code.as_deref(),
            _ => None,
        }
    }
}
