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
pub mod memory;

pub use archive::{
    ArchiveError, detect_file_type, detect_file_type_from_path, extract_tar_gz, extract_zip,
    find_entrypoint, find_entrypoint_from_metadata, normalize_extracted_dir,
};
pub use downloader::{DownloadConfig, Downloader};
pub use error::DownloadError;
pub use memory::{
    CONNECT_TIMEOUT_SECS, DEFAULT_MAX_BYTES, TIMEOUT_SECS, download_bytes_limited,
    download_text_limited, shared_client,
};

fn validate_download_filename(filename: &str) -> Result<String, DownloadError> {
    let path = std::path::Path::new(filename);
    let is_single_component = matches!(
        path.components().next(),
        Some(std::path::Component::Normal(_))
    ) && path.components().count() == 1;
    if filename.is_empty()
        || !is_single_component
        || filename.contains(['/', '\\'])
        || filename.chars().any(char::is_control)
        || path.file_name().and_then(std::ffi::OsStr::to_str) != Some(filename)
    {
        return Err(DownloadError::InvalidUrl(
            "response contains an unsafe download filename".to_string(),
        ));
    }
    Ok(filename.to_string())
}

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
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| DownloadError::InvalidUrl(format!("invalid download URL: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(DownloadError::InvalidUrl(format!(
            "download URL must use HTTP(S): {url}"
        )));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| DownloadError::Http(format!("http client: {}", e)))?;

    // 发送 HEAD 请求获取 Content-Disposition
    let response = client
        .head(parsed.clone())
        .send()
        .await
        .map_err(|e| DownloadError::Http(format!("HEAD {}: {}", url, e)))?;

    // 1. 尝试从 Content-Disposition 获取
    if let Some(cd) = response.headers().get("content-disposition")
        && let Ok(cd_str) = cd.to_str()
        && let Some(filename) = parse_content_disposition(cd_str)
    {
        return validate_download_filename(&filename);
    }

    // 2. 从结构化 URL 路径提取，避免把 query/fragment 误当作文件名。
    let filename = parsed
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|value| !value.is_empty())
        .unwrap_or("package.tar.gz");
    validate_download_filename(filename)
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

#[cfg(test)]
mod filename_tests {
    use super::validate_download_filename;

    #[test]
    fn accepts_a_single_safe_filename() {
        assert_eq!(
            validate_download_filename("agent-linux-amd64.tar.gz").expect("safe filename"),
            "agent-linux-amd64.tar.gz"
        );
    }

    #[test]
    fn rejects_paths_and_control_characters() {
        for filename in [
            "../agent.tar.gz",
            "nested/agent.tar.gz",
            "/tmp/agent.tar.gz",
            "agent\\payload.zip",
            "agent\n.zip",
            "",
        ] {
            assert!(
                validate_download_filename(filename).is_err(),
                "{filename:?}"
            );
        }
    }
}
