//! Download error types

use thiserror::Error;

/// Download operation error
#[derive(Debug, Error)]
pub enum DownloadError {
    /// HTTP request failed
    ///
    /// R11：`status` 为结构化状态码——`Some(code)` = 服务端返回了响应
    /// （重试判定按 4xx/5xx 分类）；`None` = 连接级错误（可重试）。
    /// 文案不再承载分类信息（`msg.contains("HTTP 4")` 随 URL/措辞漂移）。
    #[error("HTTP error: {message}")]
    Http {
        message: String,
        status: Option<u16>,
    },

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
            // 4xx = 客户端错误（重试同请求无意义）；5xx/连接级可重试
            Self::Http {
                status: Some(code), ..
            } => *code / 100 != 4,
            Self::Http { status: None, .. } => true,
            Self::Io(_) | Self::StreamTruncated => true,
            Self::ChecksumMismatch { .. } => true,
            _ => false,
        }
    }
}


#[cfg(test)]
mod r11_tests {
    use super::*;

    /// R11：重试判定按结构化状态码——4xx 不可重试、5xx/连接级可重试；
    /// 文案不再参与分类（URL 恰含 "HTTP 4" 字样不再误判）。
    #[test]
    fn retry_classification_uses_structured_status() {
        let not_found = DownloadError::Http {
            message: "GET https://x/HTTP 404-page: HTTP 404".into(),
            status: Some(404),
        };
        assert!(!not_found.is_retryable(), "4xx must not be retryable");

        let server_error = DownloadError::Http {
            message: "GET /x".into(),
            status: Some(503),
        };
        assert!(server_error.is_retryable(), "5xx must be retryable");

        let connection_level = DownloadError::Http {
            // 反例：文案恰含 "HTTP 4" 但无状态码（连接级）→ 仍可重试
            message: "connection reset while fetching HTTP 4xx doc page".into(),
            status: None,
        };
        assert!(connection_level.is_retryable());
    }
}
