//! file-server 二进制入口。

use std::sync::Arc;

use axum::{Router, routing::get};
use file_server::{AppState, Config, LocalWorkspaceResolver, handler};
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

    let state = AppState {
        resolver: Arc::new(LocalWorkspaceResolver::from_env()),
        config: Arc::new(Config::from_env()),
    };

    let app = Router::<AppState>::new()
        .route("/health", get(handler::health))
        .nest("/api/project", file_server::routes::project_api_router())
        .nest("/api/git", file_server::routes::git_router())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("file-server listening on {addr}");

    axum::serve(listener, app).await?;
    Ok(())
}
