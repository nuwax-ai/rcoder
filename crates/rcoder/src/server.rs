//! HTTP 服务器启动与连接管理

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

    info!(" config HTTP max_buf_size = 128KB (to prevent HTTP 431 error)");

    let app = app.into_make_service();
    let mut shutdown_rx_clone = shutdown_tx.subscribe();

    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx_clone.recv() => {
                    info!(" HTTP server closed");
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            let mut app_clone = app.clone();

                            tokio::spawn(async move {
                                let mut http_builder = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
                                http_builder
                                    .http1()
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
                                                    && !is_benign_client_disconnect(&*e) {
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

/// 判断 `serve_connection` 的错误是否属于"客户端正常断开"这类噪音。
///
/// 原先是 `e.to_string().contains("connection closed") || contains("early eof")`，
/// 两个条件各有问题：
/// - `early eof` **恒不成立**。那是 tokio `ReadExactError::EarlyEof` 的 Display 文案，
///   在 hyper 0.14 时代经 `Error` 的 cause 拼进消息里；hyper 1.x 把 Display 改成只输出
///   `description()` 的静态 kind 文案、不再拼接 cause，于是这半个过滤器随升级静默失效。
/// - `connection closed` 只是**偶然**命中 `Kind::IncompleteMessage` 的文案
///   "connection closed before message completed"，依赖的是 hyper 内部措辞。
///
/// 现改为 downcast 类型判定：`is_incomplete_message()` 与原 `connection closed` 严格等价
/// （hyper 的 kind 文案里没有第二条含该子串），`early eof` 的原意则还原成遍历 cause 链
/// 找 `io::ErrorKind::UnexpectedEof`。注意这只影响 debug 级日志的噪音量，不改变错误处理。
fn is_benign_client_disconnect(err: &(dyn std::error::Error + Send + Sync + 'static)) -> bool {
    if err
        .downcast_ref::<hyper::Error>()
        .is_some_and(|e| e.is_incomplete_message())
    {
        return true;
    }

    std::iter::successors(err.source(), |s| s.source()).any(|s| {
        s.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof)
    })
}

#[cfg(test)]
mod tests {
    use super::is_benign_client_disconnect;

    /// 模拟 hyper `Kind::Io`：自身文案不含任何关键字，真因在 cause 链里。
    /// 字符串匹配版对这种错误恒不命中，正是升级 hyper 1.x 后静默失效的形态。
    #[derive(Debug)]
    struct WrappedIo(std::io::Error);

    impl std::fmt::Display for WrappedIo {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("connection error")
        }
    }

    impl std::error::Error for WrappedIo {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn suppresses_unexpected_eof_in_cause_chain() {
        let err = WrappedIo(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "early eof",
        ));
        assert!(is_benign_client_disconnect(&err));
    }

    #[test]
    fn keeps_unrelated_io_errors() {
        let err = WrappedIo(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "permission denied",
        ));
        assert!(!is_benign_client_disconnect(&err));
    }
}
