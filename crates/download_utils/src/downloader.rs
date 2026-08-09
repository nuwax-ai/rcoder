//! HTTP downloader with retry, resume, and SHA-256 verification

use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::error::DownloadError;

static HTTP_CLIENT: tokio::sync::OnceCell<reqwest::Client> = tokio::sync::OnceCell::const_new();

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
        let parsed_url = reqwest::Url::parse(url)
            .map_err(|error| DownloadError::InvalidUrl(format!("{url}: {error}")))?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(DownloadError::InvalidUrl(url.to_string()));
        }

        let request_timeout = Duration::from_secs(self.config.timeout_secs.max(1));
        let client = if let Some(client) = HTTP_CLIENT.get() {
            // Client::clone() only clones an internal Arc and preserves the shared connection pool.
            client.clone()
        } else {
            // reqwest requests are asynchronous, but Client::build() is synchronous and may inspect
            // system proxy/TLS settings. Initialize it off the runtime worker and reuse its connection
            // pool across all downloads. The detached initializer is allowed to finish after a caller
            // cancels so the next download does not repeat the expensive setup.
            let client_task = tokio::spawn(initialize_http_client());
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    self.cleanup_cancelled_download(dest_path).await;
                    return Err(DownloadError::Cancelled);
                }
                result = tokio::time::timeout(request_timeout, client_task) => {
                    result
                        .map_err(|_| DownloadError::Http("http client construction timed out".to_string()))?
                        .map_err(|error| DownloadError::Http(format!("http client task failed: {error}")))?
                        .map_err(|error| DownloadError::Http(format!("http client: {error}")))?
                }
            }
        };

        let max_attempts = self.config.max_retries.max(1);
        for attempt in 1..=max_attempts {
            if cancel_token.is_cancelled() {
                self.cleanup_cancelled_download(dest_path).await;
                return Err(DownloadError::Cancelled);
            }

            // Check existing bytes for resume
            let mut downloaded = match tokio::fs::metadata(dest_path).await {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => return Err(DownloadError::Io(error)),
            };

            // Build request with optional Range header
            let mut req = client.get(parsed_url.clone()).timeout(request_timeout);
            if downloaded > 0 {
                info!("[download] resume: url={}, from_byte={}", url, downloaded);
                req = req.header("Range", format!("bytes={}-", downloaded));
            }

            let response = match tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    self.cleanup_cancelled_download(dest_path).await;
                    return Err(DownloadError::Cancelled);
                }
                response = req.send() => response,
            } {
                Ok(r) => r,
                Err(e) => {
                    let err = DownloadError::Http(format!("GET {}: {}", url, e));
                    if err.is_retryable() && attempt < max_attempts {
                        warn!(
                            "[download] attempt {} failed: {}, retrying...",
                            attempt, err
                        );
                        self.wait_before_retry(attempt, cancel_token, dest_path)
                            .await?;
                        continue;
                    }
                    return Err(err);
                }
            };

            // Follow redirects
            let response = match self
                .follow_redirects(&client, response, 5, request_timeout, cancel_token)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    if matches!(&e, DownloadError::Cancelled) {
                        self.cleanup_cancelled_download(dest_path).await;
                        return Err(e);
                    }
                    if e.is_retryable() && attempt < max_attempts {
                        warn!("[download] attempt {} failed: {}, retrying...", attempt, e);
                        self.wait_before_retry(attempt, cancel_token, dest_path)
                            .await?;
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
                if attempt < max_attempts {
                    warn!(
                        "[download] attempt {} failed: {}, retrying...",
                        attempt, err
                    );
                    self.wait_before_retry(attempt, cancel_token, dest_path)
                        .await?;
                    continue;
                }
                return Err(err);
            }

            if let Some(content_length) = response.content_length() {
                let expected_total = if status == reqwest::StatusCode::OK {
                    content_length
                } else {
                    downloaded.saturating_add(content_length)
                };
                if expected_total > self.config.max_bytes {
                    if let Err(e) = tokio::fs::remove_file(dest_path).await {
                        warn!(
                            "[download] failed to remove temp file {}: {e}",
                            dest_path.display()
                        );
                    }
                    return Err(DownloadError::BinaryTooLarge {
                        size: expected_total,
                        max: self.config.max_bytes,
                    });
                }
            }

            // 200 with existing bytes → server doesn't support resume, restart
            let append = if status == reqwest::StatusCode::OK && downloaded > 0 {
                info!("[download] server does not support resume, restarting");
                tokio::fs::File::create(dest_path)
                    .await
                    .map_err(DownloadError::Io)?;
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
                    break;
                }
                Err(e) => {
                    if e.is_retryable() && attempt < max_attempts {
                        warn!("[download] attempt {} failed: {}, retrying...", attempt, e);
                        self.wait_before_retry(attempt, cancel_token, dest_path)
                            .await?;
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        // Final file size check
        let file_size = tokio::fs::metadata(dest_path)
            .await
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
            let hash_path = dest_path.to_path_buf();
            let hash_task = tokio::task::spawn_blocking(move || sha256_file(&hash_path));
            let actual = tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    self.cleanup_cancelled_download(dest_path).await;
                    return Err(DownloadError::Cancelled);
                }
                result = hash_task => {
                    result
                        .map_err(|error| DownloadError::Http(format!("hash task failed: {error}")))??
                }
            };
            if actual != expected {
                // Delete file on checksum mismatch
                if let Err(e) = tokio::fs::remove_file(dest_path).await {
                    warn!("[download] failed to remove temp file on checksum mismatch: {e}");
                }
                return Err(DownloadError::ChecksumMismatch {
                    expected: expected.to_string(),
                    actual,
                });
            }
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
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dest_path)
                .await
                .map_err(DownloadError::Io)?
        } else {
            tokio::fs::File::create(dest_path)
                .await
                .map_err(DownloadError::Io)?
        };

        let mut total = initial_offset;
        let mut stream = response.bytes_stream();

        loop {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    info!("[download] download cancelled");
                    drop(file);
                    if let Err(e) = tokio::fs::remove_file(dest_path).await {
                        warn!("[download] failed to remove temp file on cancel: {e}");
                    }
                    return Err(DownloadError::Cancelled);
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            total += bytes.len() as u64;
                            if total > self.config.max_bytes {
                                drop(file);
                                if let Err(e) = tokio::fs::remove_file(dest_path).await {
                                    warn!("[download] failed to remove temp file on size limit: {e}");
                                }
                                return Err(DownloadError::BinaryTooLarge {
                                    size: total,
                                    max: self.config.max_bytes,
                                });
                            }
                            file.write_all(&bytes).await.map_err(DownloadError::Io)?;
                        }
                        Some(Err(e)) => {
                            return Err(DownloadError::Http(format!("read body: {}", e)));
                        }
                        None => break, // Download complete
                    }
                }
            }
        }
        file.flush().await.map_err(DownloadError::Io)?;

        Ok(total)
    }

    /// Follow redirects manually, accepting absolute or relative HTTP(S) targets.
    async fn follow_redirects(
        &self,
        client: &reqwest::Client,
        mut response: reqwest::Response,
        max_redirects: usize,
        request_timeout: Duration,
        cancel_token: &CancellationToken,
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
            let next_url = response.url().join(location).map_err(|error| {
                DownloadError::InvalidUrl(format!("invalid redirect URL {location}: {error}"))
            })?;
            if !matches!(next_url.scheme(), "http" | "https") {
                return Err(DownloadError::InvalidUrl(format!(
                    "redirect to non-http scheme: {}",
                    next_url
                )));
            }
            response = tokio::select! {
                biased;
                _ = cancel_token.cancelled() => return Err(DownloadError::Cancelled),
                response = client.get(next_url.clone()).timeout(request_timeout).send() => {
                    response.map_err(|e| DownloadError::Http(format!("redirect GET {next_url}: {e}")))?
                }
            };
        }
        if response.status().is_redirection() {
            return Err(DownloadError::TooManyRedirects(max_redirects));
        }
        Ok(response)
    }

    async fn wait_before_retry(
        &self,
        attempt: usize,
        cancel_token: &CancellationToken,
        dest_path: &Path,
    ) -> Result<(), DownloadError> {
        let backoff = Duration::from_secs(
            self.config.retry_backoff_base_secs * 2u64.pow((attempt as u32 - 1).min(30)),
        );
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => {
                self.cleanup_cancelled_download(dest_path).await;
                Err(DownloadError::Cancelled)
            }
            () = tokio::time::sleep(backoff) => Ok(()),
        }
    }

    async fn cleanup_cancelled_download(&self, dest_path: &Path) {
        if let Err(error) = tokio::fs::remove_file(dest_path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(path = %dest_path.display(), %error, "failed to remove cancelled download");
        }
    }
}

async fn initialize_http_client() -> Result<reqwest::Client, String> {
    HTTP_CLIENT
        .get_or_try_init(|| async {
            tokio::task::spawn_blocking(|| {
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
            })
            .await
            .map_err(|error| format!("client builder task failed: {error}"))?
            .map_err(|error| error.to_string())
        })
        .await
        .cloned()
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
                .download_to_file(
                    "ftp://example.com/file",
                    Path::new("/tmp/test"),
                    None,
                    &cancel,
                )
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
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_sha256_file_empty() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let file_path = tmp_dir.path().join("empty.txt");
        std::fs::write(&file_path, b"").unwrap();

        let hash = sha256_file(&file_path).unwrap();
        // SHA-256 of empty string
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
