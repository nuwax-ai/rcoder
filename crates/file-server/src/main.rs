//! file-server 二进制入口。

use std::sync::Arc;

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use axum::{Router, routing::get};
use file_server::error::{AppError, REQUEST_ID, generate_request_id};
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
        // 未匹配路由 → JSON ResourceError (对齐 nuwax notFoundHandler)
        .fallback(not_found)
        .layer(from_fn(request_id_layer))
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

/// 请求中间件: 生成 requestId → task_local scope 注入 (error 响应体回填) +
/// 响应头 `X-Request-Id` (对齐 nuwax req.requestId + 日志关联)。
async fn request_id_layer(req: Request, next: Next) -> Response {
    let id = generate_request_id();
    let resp = REQUEST_ID.scope(id.clone(), next.run(req)).await;
    let mut resp = resp;
    if let Ok(v) = HeaderValue::from_str(&id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

/// 未匹配路由兜底 (对齐 nuwax notFoundHandler): JSON ResourceError
/// `{success:false, code:"UNKNOWN_ERROR", error:{type:"RESOURCE_ERROR", message:"Path not found: {path}", ...}}`。
async fn not_found(req: Request) -> Response {
    let path = req.uri().path().to_string();
    AppError::resource(format!("Path not found: {path}")).into_response()
}

/// 接收 SIGINT / SIGTERM → 优雅停止所有 dev server (SIGTERM→等→SIGKILL + 还端口 + 清日志)。
/// 信号处理器安装失败时降级为仅 ctrl_c;全程无 unwrap/expect (生产规范)。
async fn shutdown_signal(dev_server: Arc<DevServerManager>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
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
