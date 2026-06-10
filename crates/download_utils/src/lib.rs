//! Download utilities with retry, resume, and SHA-256 verification
//!
//! This crate provides a robust HTTP download implementation that supports:
//! - Automatic retry with exponential backoff
//! - HTTP Range-based resume (断点续传)
//! - SHA-256 checksum verification
//! - CancellationToken for aborting downloads
//! - Streaming writes to avoid memory exhaustion
//!
//! ## Usage
//!
//! ```rust,no_run
//! use download_utils::{Downloader, DownloadConfig};
//! use tokio_util::sync::CancellationToken;
//!
//! # async fn example() -> Result<(), download_utils::DownloadError> {
//! let downloader = Downloader::new(DownloadConfig::default());
//! let cancel = CancellationToken::new();
//!
//! downloader.download_to_file(
//!     "https://example.com/file.tar.gz",
//!     "/tmp/file.tar.gz",
//!     None,  // no SHA-256 check
//!     &cancel,
//! ).await?;
//! # Ok(())
//! # }
//! ```

pub mod downloader;
pub mod error;

pub use downloader::{DownloadConfig, Downloader};
pub use error::DownloadError;
