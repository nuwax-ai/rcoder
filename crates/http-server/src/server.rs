//! HTTP 服务器启动与连接管理

use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use tracing::{error, info};

pub async fn start_http_server(
    app: axum::Router,
    port: u16,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    // bind 地址：默认 0.0.0.0（容器形态）；deploy-host 宿主机形态默认收紧为
    // 127.0.0.1（docker.sock 等价 root 的安全边界），env RCODER_BIND_HOST 可覆盖。
    #[cfg(feature = "deploy-host")]
    let default_bind_host = if shared_types::is_deploy_host() {
        "127.0.0.1"
    } else {
        "0.0.0.0"
    };
    #[cfg(not(feature = "deploy-host"))]
    let default_bind_host = "0.0.0.0";
    let bind_host = std::env::var("RCODER_BIND_HOST")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_bind_host.to_owned());
    let listener = tokio::net::TcpListener::bind(format!("{}:{}", bind_host, port))
        .await
        .map_err(|e| anyhow::anyhow!("HTTP server failed to bind port {}: {}", port, e))?;

    info!("Server starting on port {}", port);
    info!("API endpoints:");
    info!("  POST /chat - Send chat message to AI agent (legacy)");
    info!("  GET  /progress/:session_id - SSE progress stream for AI tasks (unified stream)");
    info!("  GET  /health - Health check");
    info!("  NOTE: Plan data is delivered via the unified /progress/{{session_id}} SSE stream");

    info!(" config HTTP max_buf_size = 128KB (to prevent HTTP 431 error)");

    Ok(spawn_http_listener(listener, app, shutdown_tx.subscribe()))
}

fn spawn_http_listener(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    mut shutdown_rx_clone: tokio::sync::broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    let app = app.into_make_service();
    tokio::spawn(async move {
        let closing = tokio_util::sync::CancellationToken::new();
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = shutdown_rx_clone.recv() => {
                    closing.cancel();
                    info!(" HTTP server closed");
                    break;
                }
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(error) = result { tracing::warn!("HTTP connection task failed: {error}"); }
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            let mut app_clone = app.clone();

                            let closing = closing.clone();
                            connections.spawn(async move {
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
                                                let connection = http_builder.serve_connection(io, hyper_service);
                                                tokio::pin!(connection);
                                                let result = tokio::select! {
                                                    result = &mut connection => result,
                                                    _ = closing.cancelled() => {
                                                        connection.as_mut().graceful_shutdown();
                                                        connection.await
                                                    }
                                                };
                                                if let Err(e) = result
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
        drop(listener);
        while connections.join_next().await.is_some() {}
    })
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

    #[tokio::test]
    async fn shutdown_waits_for_admitted_handler_and_closes_keep_alive() {
        use http_body_util::{BodyExt, Empty};
        use std::{sync::Arc, time::Duration};
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let route = axum::Router::new().route(
            "/write",
            axum::routing::post({
                let entered = entered.clone();
                let release = release.clone();
                move || {
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        "committed"
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);
        let mut server = super::spawn_http_listener(listener, route, rx);
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .unwrap();
        let client = tokio::spawn(connection);
        let request = || {
            hyper::Request::builder()
                .method("POST")
                .uri("/write")
                .body(Empty::<bytes::Bytes>::new())
                .unwrap()
        };
        let response = sender.send_request(request());
        tokio::pin!(response);
        // Poll the request while waiting for the controlled write to start.
        tokio::select! {
            _ = entered.notified() => {},
            result = &mut response => panic!("write completed before release: {result:?}"),
            _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("handler not entered"),
        }
        shutdown.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut server)
                .await
                .is_err()
        );
        release.notify_one();
        let response = tokio::time::timeout(Duration::from_secs(2), response)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "committed"
        );
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), client)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(sender.send_request(request()).await.is_err());
    }

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
