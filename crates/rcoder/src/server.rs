//! HTTP 服务器启动与连接管理

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use tracing::{error, info};

pub async fn start_http_server(
    app: axum::Router,
    port: u16,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .map_err(|e| anyhow::anyhow!("HTTP server failed to bind port {}: {}", port, e))?;

    info!("Server starting on port {}", port);
    info!("API endpoints:");
    info!("  POST /chat - Send chat message to AI agent (legacy)");
    info!("  GET  /progress/:session_id - SSE progress stream for AI tasks (unified stream)");
    info!("  GET  /health - Health check");
    info!("  NOTE: Plan data is delivered via the unified /progress/{{session_id}} SSE stream");

    info!("🔧 config HTTP max_buf_size = 128KB (to prevent HTTP 431 error)");

    let app = app.into_make_service();
    let mut shutdown_rx_clone = shutdown_tx.subscribe();

    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx_clone.recv() => {
                    info!("🛑 HTTP server closed");
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            let mut app_clone = app.clone();

                            tokio::spawn(async move {
                                let mut http_builder = http1::Builder::new();
                                http_builder
                                    .max_buf_size(128 * 1024)
                                    .preserve_header_case(true)
                                    .title_case_headers(false);

                                let io = TokioIo::new(stream);

                                use tower::Service;
                                match std::future::poll_fn(|cx| {
                                    Service::<std::net::SocketAddr>::poll_ready(&mut app_clone, cx)
                                }).await {
                                    Ok(()) => {
                                        match Service::<std::net::SocketAddr>::call(&mut app_clone, addr).await {
                                            Ok(service) => {
                                                let hyper_service = TowerToHyperService::new(service);
                                                if let Err(e) = http_builder.serve_connection(io, hyper_service).await
                                                    && !e.to_string().contains("connection closed")
                                                    && !e.to_string().contains("early eof") {
                                                    tracing::debug!("HTTP connection error ({}): {}", addr, e);
                                                }
                                            }
                                            Err(_) => {
                                                // Infallible 类型，不会发生
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!("server error: {}", e);
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            error!("connection failed: {}", e);
                        }
                    }
                }
            }
        }
    });

    Ok(handle)
}
