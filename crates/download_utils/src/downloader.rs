//! HTTP downloader with retry, resume, and SHA-256 verification

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::error::DownloadError;

/// Download configuration
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// Maximum download size in bytes
    pub max_bytes: u64,
    /// Download timeout in seconds
    pub timeout_secs: u64,
    /// Maximum retry attempts
    pub max_retries: usize,
    /// Retry backoff base in seconds (exponential: base * 2^attempt)
    pub retry_backoff_base_secs: u64,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            max_bytes: 500 * 1024 * 1024, // 500MB
            timeout_secs: 600,            // 10 minutes
            max_retries: 3,
            retry_backoff_base_secs: 1,
        }
    }
}

/// HTTP downloader with retry, resume, and SHA-256 verification
pub struct Downloader {
    config: DownloadConfig,
}

impl Downloader {
    /// Create a new downloader with default config
    pub fn new(config: DownloadConfig) -> Self {
        Self { config }
    }

    /// Get the config
    pub fn config(&self) -> &DownloadConfig {
        &self.config
    }

    /// Download URL content to file with retry, resume, and optional SHA-256 verification
    ///
    /// # Arguments
    /// * `url` - HTTP/HTTPS URL to download
    /// * `dest_path` - Destination file path
    /// * `expected_sha256` - Optional SHA-256 hex string for verification
    /// * `cancel_token` - Token to cancel the download
    ///
    /// # Returns
    /// Total bytes downloaded
    pub async fn download_to_file(
        &self,
        url: &str,
        dest_path: &Path,
        expected_sha256: Option<&str>,
        cancel_token: &CancellationToken,
    ) -> Result<u64, DownloadError> {
        // Validate URL scheme
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(DownloadError::InvalidUrl(url.to_string()));
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.config.timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| DownloadError::Http(format!("http client: {}", e)))?;

        let mut last_err: Option<DownloadError> = None;

        for attempt in 1..=self.config.max_retries {
            // Check existing bytes for resume
            let mut downloaded = if dest_path.exists() {
                std::fs::metadata(dest_path)
                    .map(|m| m.len())
                    .unwrap_or(0)
            } else {
                0
            };

            // Build request with optional Range header
            let mut req = client.get(url);
            if downloaded > 0 {
                info!(
                    "[download] resume: url={}, from_byte={}",
                    url, downloaded
                );
                req = req.header("Range", format!("bytes={}-", downloaded));
            }

            let response = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    let err = DownloadError::Http(format!("GET {}: {}", url, e));
                    if err.is_retryable() && attempt < self.config.max_retries {
                        warn!("[download] attempt {} failed: {}, retrying...", attempt, err);
                        last_err = Some(err);
                        let backoff = Duration::from_secs(
                            self.config.retry_backoff_base_secs * 2u64.pow(attempt as u32 - 1),
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(err);
                }
            };

            // Follow redirects
            let response = match self.follow_redirects(&client, response, 5).await {
                Ok(r) => r,
                Err(e) => {
                    if e.is_retryable() && attempt < self.config.max_retries {
                        warn!("[download] attempt {} failed: {}, retrying...", attempt, e);
                        last_err = Some(e);
                        let backoff = Duration::from_secs(
                            self.config.retry_backoff_base_secs * 2u64.pow(attempt as u32 - 1),
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(e);
                }
            };

            let status = response.status();

            // 416 Range Not Satisfiable → file already complete
            if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                info!("[download] file already complete: url={}", url);
                break;
            }

            // 4xx (non-416) → no retry
            if status.is_client_error() {
                return Err(DownloadError::Http(format!("GET {}: HTTP {}", url, status)));
            }

            // 5xx → retryable
            if status.is_server_error() {
                let err = DownloadError::Http(format!("GET {}: HTTP {}", url, status));
                if attempt < self.config.max_retries {
                    warn!("[download] attempt {} failed: {}, retrying...", attempt, err);
                    last_err = Some(err);
                    let backoff = Duration::from_secs(
                        self.config.retry_backoff_base_secs * 2u64.pow(attempt as u32 - 1),
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                return Err(err);
            }

            // 200 with existing bytes → server doesn't support resume, restart
            let append = if status == reqwest::StatusCode::OK && downloaded > 0 {
                info!("[download] server does not support resume, restarting");
                std::fs::File::create(dest_path).map_err(DownloadError::Io)?;
                downloaded = 0;
                false
            } else {
                downloaded > 0 // 206 → append
            };

            // Stream response to file
            let result = self
                .write_response_to_file(response, dest_path, downloaded, append, cancel_token)
                .await;

            match result {
                Ok(total_bytes) => {
                    info!(
                        "[download] complete: url={}, bytes={}, attempt={}",
                        url, total_bytes, attempt
                    );
                    last_err = None;
                    break;
                }
                Err(e) => {
                    if e.is_retryable() && attempt < self.config.max_retries {
                        warn!("[download] attempt {} failed: {}, retrying...", attempt, e);
                        last_err = Some(e);
                        let backoff = Duration::from_secs(
                            self.config.retry_backoff_base_secs * 2u64.pow(attempt as u32 - 1),
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        // Final file size check
        let file_size = std::fs::metadata(dest_path)
            .map(|m| m.len())
            .map_err(DownloadError::Io)?;

        if file_size > self.config.max_bytes {
            return Err(DownloadError::BinaryTooLarge {
                size: file_size,
                max: self.config.max_bytes,
            });
        }

        // SHA-256 verification
        if let Some(expected) = expected_sha256.filter(|s| !s.is_empty()) {
            let actual = sha256_file(dest_path)?;
            if actual != expected {
                // Delete file on checksum mismatch
                let _ = std::fs::remove_file(dest_path);
                return Err(DownloadError::ChecksumMismatch {
                    expected: expected.to_string(),
                    actual,
                });
            }
        }

        if let Some(err) = last_err {
            return Err(err);
        }

        Ok(file_size)
    }

    /// Stream HTTP response body to file
    async fn write_response_to_file(
        &self,
        response: reqwest::Response,
        dest_path: &Path,
        initial_offset: u64,
        append: bool,
        cancel_token: &CancellationToken,
    ) -> Result<u64, DownloadError> {
        let mut file = if append {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dest_path)
                .map_err(DownloadError::Io)?
        } else {
            std::fs::File::create(dest_path).map_err(DownloadError::Io)?
        };

        let mut total = initial_offset;
        let mut stream = response.bytes_stream();

        loop {
            tokio::select! {
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            total += bytes.len() as u64;
                            if total > self.config.max_bytes {
                                let _ = std::fs::remove_file(dest_path);
                                return Err(DownloadError::BinaryTooLarge {
                                    size: total,
                                    max: self.config.max_bytes,
                                });
                            }
                            file.write_all(&bytes).map_err(DownloadError::Io)?;
                        }
                        Some(Err(e)) => {
                            return Err(DownloadError::Http(format!("read body: {}", e)));
                        }
                        None => break, // Download complete
                    }
                }
                _ = cancel_token.cancelled() => {
                    info!("[download] download cancelled");
                    let _ = std::fs::remove_file(dest_path);
                    return Err(DownloadError::Cancelled);
                }
            }
        }
        file.flush().map_err(DownloadError::Io)?;

        Ok(total)
    }

    /// Follow redirects manually, validating each target URL is http/https
    async fn follow_redirects(
        &self,
        client: &reqwest::Client,
        mut response: reqwest::Response,
        max_redirects: usize,
    ) -> Result<reqwest::Response, DownloadError> {
        for _ in 0..max_redirects {
            if !response.status().is_redirection() {
                return Ok(response);
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or(DownloadError::RedirectMissingLocation)?;
            if !location.starts_with("http://") && !location.starts_with("https://") {
                return Err(DownloadError::InvalidUrl(format!(
                    "redirect to non-http scheme: {}",
                    location
                )));
            }
            response = client
                .get(location)
                .send()
                .await
                .map_err(|e| DownloadError::Http(format!("redirect GET {}: {}", location, e)))?;
        }
        if response.status().is_redirection() {
            return Err(DownloadError::TooManyRedirects(max_redirects));
        }
        Ok(response)
    }
}

/// Calculate SHA-256 hex digest of a file
fn sha256_file(path: &Path) -> Result<String, DownloadError> {
    use std::io::Read;
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path).map_err(DownloadError::Io)?;
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).map_err(DownloadError::Io)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_config_default() {
        let config = DownloadConfig::default();
        assert_eq!(config.max_bytes, 500 * 1024 * 1024);
        assert_eq!(config.timeout_secs, 600);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.retry_backoff_base_secs, 1);
    }

    #[test]
    fn test_invalid_url_scheme() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let downloader = Downloader::new(DownloadConfig::default());
            let cancel = CancellationToken::new();
            let result = downloader
                .download_to_file("ftp://example.com/file", Path::new("/tmp/test"), None, &cancel)
                .await;
            assert!(matches!(result, Err(DownloadError::InvalidUrl(_))));
        });
    }

    #[test]
    fn test_invalid_url_scheme_file() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let downloader = Downloader::new(DownloadConfig::default());
            let cancel = CancellationToken::new();
            let result = downloader
                .download_to_file("file:///etc/passwd", Path::new("/tmp/test"), None, &cancel)
                .await;
            assert!(matches!(result, Err(DownloadError::InvalidUrl(_))));
        });
    }

    #[test]
    fn test_sha256_file() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let file_path = tmp_dir.path().join("test_file.txt");
        std::fs::write(&file_path, b"hello world").unwrap();

        let hash = sha256_file(&file_path).unwrap();
        // SHA-256 of "hello world"
        assert_eq!(hash, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
    }

    #[test]
    fn test_sha256_file_empty() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let file_path = tmp_dir.path().join("empty.txt");
        std::fs::write(&file_path, b"").unwrap();

        let hash = sha256_file(&file_path).unwrap();
        // SHA-256 of empty string
        assert_eq!(hash, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }
}
