//! Agent 下载错误类型
//!
//! 复用 download_utils::DownloadError 作为基础，添加业务特有的错误变体。

use thiserror::Error;

/// Agent 下载操作错误
///
/// 基础下载错误来自 download_utils::DownloadError，
/// 业务特有错误在此定义。
#[derive(Debug, Error)]
pub enum AgentDownloadError {
    /// 基础下载错误（来自 download_utils）
    #[error(transparent)]
    Download(#[from] download_utils::DownloadError),

    /// IO 错误
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON 序列化/反序列化错误
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Agent 版本已安装
    #[error("version already installed: {agent_id}@{version}")]
    VersionAlreadyInstalled { agent_id: String, version: String },

    /// Agent 未找到
    #[error("agent not found: {0}")]
    NotFound(String),

    /// 无效的 manifest
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),

    /// 平台未找到
    #[error("platform not found: {0}")]
    PlatformNotFound(String),

    /// 安装失败
    #[error("install failed: {0}")]
    InstallFailed(String),
}

impl AgentDownloadError {
    /// 判断是否可重试
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Download(e) => e.is_retryable(),
            Self::InstallFailed(msg) => !msg.contains("HTTP 4"),
            _ => false,
        }
    }
}
