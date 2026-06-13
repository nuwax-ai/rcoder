//! Download utilities with retry, resume, and SHA-256 verification
//!
//! This crate provides a robust HTTP download implementation that supports:
//! - Automatic retry with exponential backoff
//! - HTTP Range-based resume (断点续传)
//! - SHA-256 checksum verification
//! - CancellationToken for aborting downloads
//! - Streaming writes to avoid memory exhaustion
//! - Archive extraction (tar.gz, zip)
//!
//! ## Usage
//!
//! ```rust,no_run
//! use std::path::Path;
//! use download_utils::{Downloader, DownloadConfig};
//! use tokio_util::sync::CancellationToken;
//!
//! # async fn example() -> Result<(), download_utils::DownloadError> {
//! let downloader = Downloader::new(DownloadConfig::default());
//! let cancel = CancellationToken::new();
//!
//! downloader.download_to_file(
//!     "https://example.com/file.tar.gz",
//!     Path::new("/tmp/file.tar.gz"),
//!     None,  // no SHA-256 check
//!     &cancel,
//! ).await?;
//! # Ok(())
//! # }
//! ```

pub mod archive;
pub mod downloader;
pub mod error;

pub use archive::{ArchiveError, extract_tar_gz, extract_zip, detect_file_type, detect_file_type_from_path, normalize_extracted_dir, find_entrypoint, find_entrypoint_from_metadata};
pub use downloader::{DownloadConfig, Downloader};
pub use error::DownloadError;

/// 从 URL 获取文件名（优先从 Content-Disposition，其次从 URL 路径）
///
/// 标准做法：先尝试从 HTTP 响应的 Content-Disposition header 获取真实文件名，
/// 如果没有则从 URL 路径部分提取（去掉查询参数）。
///
/// # Arguments
/// * `url` - HTTP/HTTPS URL
///
/// # Returns
/// 文件名字符串
pub async fn get_filename_from_url(url: &str) -> Result<String, DownloadError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| DownloadError::Http(format!("http client: {}", e)))?;

    // 发送 HEAD 请求获取 Content-Disposition
    let response = client.head(url).send().await
        .map_err(|e| DownloadError::Http(format!("HEAD {}: {}", url, e)))?;

    // 1. 尝试从 Content-Disposition 获取
    if let Some(cd) = response.headers().get("content-disposition")
        && let Ok(cd_str) = cd.to_str()
        && let Some(filename) = parse_content_disposition(cd_str)
    {
        return Ok(filename);
    }

    // 2. 从 URL 路径提取（去掉查询参数）
    let path = url.split('?').next().unwrap_or(url);
    let filename = path.split('/').next_back().unwrap_or("package.tar.gz");
    Ok(filename.to_string())
}

/// 解析 Content-Disposition header 中的文件名
///
/// 支持格式：
/// - `Content-Disposition: attachment; filename="file.tar.gz"`
/// - `Content-Disposition: attachment; filename*=UTF-8''file.tar.gz` (RFC 5987)
fn parse_content_disposition(cd: &str) -> Option<String> {
    for part in cd.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename=") {
            let filename = rest.trim_matches('"');
            return Some(filename.to_string());
        }
        if let Some(rest) = part.strip_prefix("filename*=") {
            // RFC 5987: filename*=UTF-8''file.tar.gz
            let filename = rest.trim_matches('"');
            if let Some(idx) = filename.find("''") {
                return Some(filename[idx + 2..].to_string());
            }
        }
    }
    None
}
