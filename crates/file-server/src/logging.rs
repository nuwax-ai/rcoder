//! 独立二进制可选的日志初始化；嵌入式调用方可使用自己的 subscriber。

use anyhow::{Context, Result};
use tracing_appender::rolling::Rotation;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, registry::Registry};

use crate::Config;

/// 类型擦除的 tracing layer（供嵌入式注入到外部 subscriber）。
pub type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// non-blocking 文件日志的刷盘 guard（公开 API 的返回类型，须可被调用方命名/持有）。
pub use tracing_appender::non_blocking::WorkerGuard;

/// 构造 file-server 独立日志的 fmt layer + WorkerGuard（供嵌入式注入到外部 subscriber）。
///
/// per-layer filter 只收 `file_server` target（含 `file_server::http`），不依赖全局 EnvFilter。
/// 返回的 guard 必须在进程生命周期内持有，否则后台日志可能尚未刷盘。
pub fn build_file_layer(config: &Config) -> Result<(BoxedLayer, WorkerGuard)> {
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
    let layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(file_writer)
        .with_filter(
            Targets::new()
                .with_target("file_server", tracing::Level::INFO)
                .with_default(LevelFilter::OFF),
        )
        .boxed();
    Ok((layer, guard))
}

/// 初始化控制台与按日滚动文件日志。`RUST_LOG` 存在时完整覆盖默认过滤规则。
/// 返回的 guard 必须在进程生命周期内持有，否则后台日志可能尚未刷盘。
pub fn init(config: &Config) -> Result<WorkerGuard> {
    let (file_layer, guard) = build_file_layer(config)?;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("file_server=info,tower_http=info"));
    let console = tracing_subscriber::fmt::layer().with_target(true);
    tracing_subscriber::registry()
        .with(file_layer)
        .with(filter)
        .with(console)
        .try_init()
        .context("initialize file-server tracing subscriber")?;
    Ok(guard)
}
