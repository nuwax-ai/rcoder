//! pnpm 安装服务的稳定公共门面。
//!
//! 当前后端是稳定 pnpm CLI + NDJSON 协议。路由只依赖本模块公开的数据模型和
//! [`install`]；未来接入官方 Rust 引擎时，可替换或新增 backend，而不改业务调用方。

mod classify;
mod cli;
mod error;
mod protocol;
mod types;

use std::path::Path;

pub use error::{FailureKind, InstallError};
pub use types::{InstallOptions, InstallOutcome, InstallSummary, LogFiles};

/// 使用当前 pnpm 后端安装依赖。
pub async fn install(
    cwd: &Path,
    options: &InstallOptions,
    logs: Option<&LogFiles>,
    timeout_secs: u64,
) -> Result<InstallOutcome, InstallError> {
    cli::install(cwd, options, logs, timeout_secs).await
}
