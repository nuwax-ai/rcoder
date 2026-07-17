//! file-server 二进制入口。

use std::sync::Arc;

use axum::{Router, routing::get};
use file_server::{AppState, Config, DevServerManager, LocalWorkspaceResolver, handler};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

/// 默认监听端口 (对齐 nuwax `PORT=60000` / 部署侧 `FILE_SERVER_PORT`)。
const DEFAULT_PORT: u16 = 60000;
/// env: 监听端口 (部署侧 `start-services.sh` 读取)。
const ENV_PORT: &str = "FILE_SERVER_PORT";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("file_server=debug,tower_http=debug")),
        )
        .init();

    let port = std::env::var(ENV_PORT)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_PORT);

    let config = Arc::new(Config::from_env());
    let state = AppState {
        resolver: Arc::new(LocalWorkspaceResolver::from_env()),
        dev_server: Arc::new(DevServerManager::new(config.clone())),
        config,
    };
    // clone Arc 给 graceful shutdown 闭包 (state 会被 with_state 消费)
    let dev_server = state.dev_server.clone();

    let app = Router::<AppState>::new()
        .route("/health", get(handler::health))
        .nest("/api/project", file_server::routes::project_api_router())
        .nest("/api/git", file_server::routes::git_router())
        .nest("/api/build", file_server::routes::build_router())
        .nest("/api/computer", file_server::routes::computer_router())
        .nest("/api/page", file_server::routes::page_router())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("file-server listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(dev_server))
        .await?;
    Ok(())
}

/// 接收 SIGINT / SIGTERM → 优雅停止所有 dev server (SIGTERM→等→SIGKILL + 还端口 + 清日志)。
/// 信号处理器安装失败时降级为仅 ctrl_c;全程无 unwrap/expect (生产规范)。
async fn shutdown_signal(dev_server: Arc<DevServerManager>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "install SIGTERM handler failed, fallback to ctrl_c only");
                let _ = tokio::signal::ctrl_c().await;
                dev_server.shutdown_all().await;
                return;
            }
        };
        // 真正 await 信号到达 (SIGTERM 的 recv future 必须被 poll, 否则闭包会瞬间返回)
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT, shutting down dev servers"),
            _ = term.recv() => tracing::info!("received SIGTERM, shutting down dev servers"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("received interrupt, shutting down dev servers");
    }
    dev_server.shutdown_all().await;
}
