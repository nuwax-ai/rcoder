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
    let bind_host = shared_types::service_bind_host();
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

// 观测接入（dial9）：accept 外层任务与每连接任务均经 rcoder-obs 门面 spawn——
// feature 关闭时直通原 Tokio API（`tokio::spawn` / `JoinSet::spawn`），零插桩；
// 开启时记录 wake 因果与 task 生命周期，JoinSet 收集/关停逻辑不变
// （`spawn_in_join_set` 与原生同返回 AbortHandle）。
// 未覆盖（有意保留，见 docs/observability.md dial9 节）：Hyper H2 executor
// 内部任务与 Axum WebSocket/on_upgrade 自行 spawn 的应用任务不经本门面。
fn spawn_http_listener(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    mut shutdown_rx_clone: tokio::sync::broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    let app = app.into_make_service();
    rcoder_obs::spawn(async move {
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
                            rcoder_obs::spawn_in_join_set(
                                &mut connections,
                                async move {
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

    /// 128KiB max_buf_size 的头部上下界（HTTP 431 边界）：
    /// - 约 96KiB 合法单 header：请求 200 且 handler 收到完整值；
    /// - 约 160KiB（高于 128KiB、低于 hyper 默认上限）：真实 431 响应、
    ///   handler 不执行、连接关闭；
    /// - 431 之后新连接仍 200（服务未被单条坏请求拖垮）。
    /// 证据必须是真实 HTTP/1.1 响应状态码，不用断连日志/Router 指标代替。
    #[tokio::test]
    async fn header_boundary_96kib_ok_and_160kib_real_431() {
        use axum::http::HeaderMap;
        use http_body_util::{BodyExt, Empty};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let handler_ran = Arc::new(AtomicBool::new(false));
        let route = axum::Router::new().route(
            "/echo-header",
            axum::routing::get({
                let handler_ran = handler_ran.clone();
                move |headers: HeaderMap| {
                    let handler_ran = handler_ran.clone();
                    async move {
                        handler_ran.store(true, Ordering::SeqCst);
                        headers
                            .get("x-big")
                            .map(|value| value.len().to_string())
                            .unwrap_or_else(|| "missing".to_string())
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);
        let mut server = super::spawn_http_listener(listener, route, rx);

        let connect = || async {
            let stream = tokio::net::TcpStream::connect(address).await.unwrap();
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .unwrap()
        };
        let request_with = |size: usize| {
            hyper::Request::builder()
                .method("GET")
                .uri("/echo-header")
                .header("x-big", "a".repeat(size))
                .body(Empty::<bytes::Bytes>::new())
                .unwrap()
        };

        // 96KiB 单 header：低于 128KiB 缓冲 → 200，值完整到达 handler。
        let (mut sender, connection) = connect().await;
        let client = tokio::spawn(connection);
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            sender.send_request(request_with(96 * 1024)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = tokio::time::timeout(Duration::from_secs(5), response.into_body().collect())
            .await
            .unwrap()
            .unwrap()
            .to_bytes();
        assert_eq!(
            body,
            (96 * 1024).to_string(),
            "handler must see the full 96KiB value"
        );
        drop(sender);
        tokio::time::timeout(Duration::from_secs(5), client)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        // 160KiB 单 header：超过 128KiB 缓冲 → 真实 431，handler 不执行。
        handler_ran.store(false, Ordering::SeqCst);
        let (mut sender, connection) = connect().await;
        let client = tokio::spawn(connection);
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            sender.send_request(request_with(160 * 1024)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );
        assert!(
            !handler_ran.load(Ordering::SeqCst),
            "431 must be rejected before the handler runs"
        );
        // 431 后连接关闭：同连接再发请求必须失败。
        // 431 后连接关闭：同连接再发请求必须失败（连接已关时 hyper 客户端报
        // Canceled/"connection was not ready"——这正是服务端关闭的证据）。
        match tokio::time::timeout(
            Duration::from_secs(5),
            sender.send_request(request_with(16)),
        )
        .await
        {
            Err(_elapsed) => {} // 等待超时：同样只可能在连接死掉后发生
            Ok(Err(_closed)) => {}
            Ok(Ok(response)) => {
                panic!("connection must be closed after 431, got {}", response.status())
            }
        }
        drop(sender);
        let _ = tokio::time::timeout(Duration::from_secs(5), client).await;

        // 新连接恢复正常：小请求 200。
        let (mut sender, connection) = connect().await;
        let client = tokio::spawn(connection);
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            sender.send_request(request_with(16)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        drop(sender);
        tokio::time::timeout(Duration::from_secs(5), client)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }

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
