//! 独立二进制可选的日志初始化；嵌入式调用方可使用自己的 subscriber。

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::Rotation;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::Config;

/// 初始化控制台与按日滚动文件日志。`RUST_LOG` 存在时完整覆盖默认过滤规则。
/// 返回的 guard 必须在进程生命周期内持有，否则后台日志可能尚未刷盘。
pub fn init(config: &Config) -> Result<WorkerGuard> {
    std::fs::create_dir_all(&config.service_log_dir).with_context(|| {
        format!(
            "create file-server log directory {}",
            config.service_log_dir.display()
        )
    })?;
    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("file-server.log")
        .max_log_files(config.service_log_retention_days)
        .build(&config.service_log_dir)
        .context("build daily file-server log appender")?;
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("file_server=info,tower_http=info"));
    let console = tracing_subscriber::fmt::layer().with_target(true);
    let file = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(file_writer);
    tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file)
        .try_init()
        .context("initialize file-server tracing subscriber")?;
    Ok(guard)
}
