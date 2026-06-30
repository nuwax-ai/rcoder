//! Download error types

use thiserror::Error;

/// Download operation error
#[derive(Debug, Error)]
pub enum DownloadError {
    /// HTTP request failed
    #[error("HTTP error: {0}")]
    Http(String),

    /// IO error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// File exceeds maximum allowed size
    #[error("Binary too large: {size} bytes exceeds max {max}")]
    BinaryTooLarge { size: u64, max: u64 },

    /// SHA-256 checksum mismatch
    #[error("Checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },

    /// Download stream was truncated
    #[error("Stream truncated")]
    StreamTruncated,

    /// Download was cancelled via CancellationToken
    #[error("Download cancelled")]
    Cancelled,

    /// Invalid URL scheme (not http/https)
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),

    /// Too many redirects
    #[error("Too many redirects (max {0})")]
    TooManyRedirects(usize),

    /// Redirect missing Location header
    #[error("Redirect missing Location header")]
    RedirectMissingLocation,
}

impl DownloadError {
    /// Check if the error is retryable
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http(msg) => !msg.contains("HTTP 4"),
            Self::Io(_) | Self::StreamTruncated => true,
            Self::ChecksumMismatch { .. } => true,
            _ => false,
        }
    }
}
