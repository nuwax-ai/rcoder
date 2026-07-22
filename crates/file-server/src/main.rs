//! file-server 独立二进制薄入口。

use anyhow::Context;
use file_server::{Config, FileServer};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load().context("load file-server configuration")?;
    // 必须持有到 main 结束，确保 non-blocking 文件日志完整刷盘。
    let _log_guard = file_server::logging::init(&config)?;
    // 版本标识 (编译时注入, 日志确认镜像代码版本)
    tracing::info!("═══════════════════════════════════════════");
    tracing::info!(
        "🚀 file-server v{} starting — BUILD: {} @ {} (branch: {})",
        env!("CARGO_PKG_VERSION"),
        env!("RCODER_BUILD_GIT_HASH"),
        env!("RCODER_BUILD_TIME"),
        env!("RCODER_BUILD_GIT_BRANCH")
    );
    tracing::info!("═══════════════════════════════════════════");
    let address = format!("{}:{}", config.listen_host, config.port);
    let server = FileServer::builder(config).build()?;
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("bind file-server listener {address}"))?;
    tracing::info!(address, "file-server listening");
    server
        .serve_with_shutdown(listener, shutdown_signal())
        .await
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(error) => {
                tracing::error!(%error, "install SIGTERM handler failed; waiting for SIGINT");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::error!(%error, "wait for SIGINT failed");
                }
                tracing::info!("received SIGINT");
            }
            _ = term.recv() => tracing::info!("received SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "wait for interrupt failed");
        }
    }
}
