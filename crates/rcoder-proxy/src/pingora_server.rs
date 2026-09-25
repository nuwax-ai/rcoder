//! Pingora 服务器启动和管理模块
//!
//! 提供基于 Pingora 库的完整反向代理服务器启动功能，支持 HTTP/1.1 和 HTTP/2。

use crate::ProxyError;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::{error, info};

use pingora_core::Result as PingoraResult;
use pingora_core::protocols::Digest;
use pingora_core::server::Server;
use pingora_core::server::configuration::Opt;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::ResponseHeader;
use pingora_proxy::{FailToProxy, ProxyHttp};

use crate::config::ProxyConfig;
use crate::service::{PingoraProxyService, PortProxy};
use shared_types::ModelProviderConfig;

/// Pingora 服务器管理器
pub struct PingoraServerManager {
    config: ProxyConfig,
    service: Arc<PingoraProxyService>,
}

impl PingoraServerManager {
    /// 创建新的 Pingora 服务器管理器
    pub fn new(config: ProxyConfig) -> Self {
        let service = Arc::new(PingoraProxyService::new(config.clone()));
        Self { config, service }
    }

    /// 设置共享的 API 密钥管理器
    ///
    /// 这个方法允许从外部传入一个共享的 DashMap，使 agent_runner 和 Pingora
    /// 能够共享 API 密钥配置。
    ///
    /// # 参数
    ///
    /// * `api_key_manager` - 共享的 DashMap<String, ModelProviderConfig>
    pub fn with_api_key_manager(
        mut self,
        api_key_manager: Arc<DashMap<String, ModelProviderConfig>>,
    ) -> Self {
        // 由于 Arc 需要先解包再重新包装，使用 Arc::try_unwrap 或创建新的 service
        // 简单起见，我们创建新的 PingoraProxyService
        let new_service = (*self.service)
            .clone()
            .with_api_key_manager(api_key_manager);
        self.service = Arc::new(new_service);
        self
    }

    /// 设置 API Key 鉴权配置
    ///
    /// 传入共享的 API Key 配置，使 Pingora 层也能进行 API Key 验证。
    /// 使用 ArcSwap 实现无锁读取，提升并发性能。
    ///
    /// # 参数
    ///
    /// * `config` - 共享的 `Arc<ArcSwap<ApiKeyAuthConfig>>`
    pub fn with_api_key_config(
        mut self,
        config: Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>,
    ) -> Self {
        let new_service = (*self.service).clone().with_api_key_config(config);
        self.service = Arc::new(new_service);
        self
    }

    /// 设置容器查找服务（统一数据源）
    ///
    /// 注入共享的容器查找服务（通常是 `Arc<ProjectAdapter>`），
    /// 使 Pingora 代理层（/web/ttyd、/computer/ttyd 等路由）能通过
    /// user_id / project_id / pod_id 解析容器 IP，无需自己维护映射。
    ///
    /// # 参数
    ///
    /// * `lookup` - 共享的 `Arc<dyn ContainerLookup>`
    pub fn with_container_lookup(mut self, lookup: Arc<dyn shared_types::ContainerLookup>) -> Self {
        let new_service = (*self.service).clone().with_container_lookup(lookup);
        self.service = Arc::new(new_service);
        self
    }

    /// 设置 Userapp 访问追踪（闲置回收的 HTTP 访问信号源，/api/v1/userapp/proxy/app/prod/* 路由用）
    pub fn with_access_tracker(mut self, tracker: Arc<dyn shared_types::AppAccessTracker>) -> Self {
        let new_service = (*self.service).clone().with_access_tracker(tracker);
        self.service = Arc::new(new_service);
        self
    }

    /// 设置 Userapp 流量唤醒控制（stopped app 收到请求时 hold-and-wait 拉起）
    pub fn with_wake_control(mut self, wc: Arc<dyn shared_types::AppWakeControl>) -> Self {
        let new_service = (*self.service).clone().with_wake_control(wc);
        self.service = Arc::new(new_service);
        self
    }

    /// 启动 Pingora 服务器
    ///
    /// 接受一个 `shutdown_rx` 用于接收外部关闭信号。
    /// 当 `shutdown_rx` 收到信号（或 sender 被 drop）时，`start()` 返回。
    /// Pingora 服务器线程运行 `run_forever()`，由进程退出时 OS 清理。
    pub async fn start(&mut self, shutdown_rx: oneshot::Receiver<()>) -> Result<(), ProxyError> {
        info!("starting Pingora proxy server...");
        info!("listening on: 0.0.0.0:{}", self.config.listen_port);
        info!("route: /proxy/{{port}}{{/path}}");

        // 创建 Pingora 服务器配置
        let opt = Opt::default();

        // 创建 Pingora 服务器
        let mut my_server = Server::new(Some(opt))
            .map_err(|e| ProxyError::Config(format!("Failed to create Pingora server: {}", e)))?;
        my_server.bootstrap();

        // 创建代理服务实例
        let proxy_service = self.service.create_pingora_proxy().map_err(|e| {
            error!("[PINGORA] create proxy failed: {}", e);
            e
        })?;
        let proxy_service = Arc::new(proxy_service);
        // 创建 HTTP 代理服务
        let mut http_proxy = pingora_proxy::http_proxy_service(
            &my_server.configuration,
            ProxyServiceWrapper {
                inner: proxy_service.clone(),
            },
        );

        // 添加 TCP 监听器
        http_proxy.add_tcp(&format!("0.0.0.0:{}", self.config.listen_port));

        // 将服务添加到服务器
        my_server.add_service(http_proxy);

        // 在独立线程中运行服务器（使用 std::thread 而不是 spawn_blocking）
        // spawn_blocking 在某些环境下可能有调度延迟问题
        info!("created Pingora proxy service...");
        let server_thread = std::thread::spawn(move || {
            info!("Pingora proxy starting...");
            my_server.run_forever();
        });
        info!("Pingora server already created");

        // 等待外部关闭信号（sender 被 drop 或显式发送信号都会触发）
        let _ = shutdown_rx.await;
        info!("shutdown signal received, Pingora proxy cleanup by OS");

        // 不再 join 线程 — run_forever() 永不返回，join() 会导致永久阻塞
        // detach 线程，让进程退出时自动清理
        drop(server_thread);

        Ok(())
    }

    /// 获取服务引用
    pub fn service(&self) -> Arc<PingoraProxyService> {
        self.service.clone()
    }
}

/// 包装器结构体，用于实现 Pingora 的 ProxyHttp trait
struct ProxyServiceWrapper {
    inner: Arc<PortProxy>,
}

#[async_trait::async_trait]
impl ProxyHttp for ProxyServiceWrapper {
    type CTX = crate::service::TrackingCtx;

    fn new_ctx(&self) -> Self::CTX {
        crate::service::TrackingCtx::new()
    }

    async fn request_filter(
        &self,
        session: &mut pingora_proxy::Session,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<bool> {
        // 委托给内部的 PortProxy 实现（协议转换拦截）
        self.inner.request_filter(session, ctx).await
    }

    async fn upstream_peer(
        &self,
        session: &mut pingora_proxy::Session,
        _ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        // 委托给内部的 PortProxy 实现
        self.inner.upstream_peer(session, _ctx).await
    }

    fn fail_to_connect(
        &self,
        session: &mut pingora_proxy::Session,
        peer: &HttpPeer,
        ctx: &mut Self::CTX,
        error: Box<pingora_core::Error>,
    ) -> Box<pingora_core::Error> {
        self.inner.fail_to_connect(session, peer, ctx, error)
    }

    async fn fail_to_proxy(
        &self,
        session: &mut pingora_proxy::Session,
        error: &pingora_core::Error,
        ctx: &mut Self::CTX,
    ) -> FailToProxy {
        if ctx.prod_metrics_counted {
            self.inner.metrics.dec_active();
            ctx.prod_metrics_counted = false;
        }
        let recoverable_connect_error = ctx.prod_connect_recovery.is_some()
            && matches!(
                error.etype(),
                pingora_core::ErrorType::ConnectRefused
                    | pingora_core::ErrorType::ConnectNoRoute
                    | pingora_core::ErrorType::ConnectTimedout
            );
        let should_report_unavailable = recoverable_connect_error
            || ctx
                .prod_connect_recovery
                .as_ref()
                .is_some_and(|state| state.unavailable_response);
        if should_report_unavailable
            && let Ok(mut response) = ResponseHeader::build(503, None)
            && response.insert_header("Retry-After", "15").is_ok()
            && session
                .write_response_header(Box::new(response), true)
                .await
                .is_ok()
        {
            return FailToProxy {
                error_code: 503,
                can_reuse_downstream: false,
            };
        }
        self.inner.fail_to_proxy(session, error, ctx).await
    }

    async fn upstream_request_filter(
        &self,
        session: &mut pingora_proxy::Session,
        upstream_request: &mut pingora_http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        // 委托给内部的 PortProxy 实现
        self.inner
            .upstream_request_filter(session, upstream_request, ctx)
            .await
    }

    async fn connected_to_upstream(
        &self,
        session: &mut pingora_proxy::Session,
        reused: bool,
        peer: &HttpPeer,
        #[cfg(unix)] fd: std::os::unix::io::RawFd,
        #[cfg(windows)] sock: std::os::windows::io::RawSocket,
        digest: Option<&Digest>,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        // 委托给内部的 PortProxy 实现
        self.inner
            .connected_to_upstream(
                session,
                reused,
                peer,
                #[cfg(unix)]
                fd,
                #[cfg(windows)]
                sock,
                digest,
                ctx,
            )
            .await
    }

    async fn response_filter(
        &self,
        session: &mut pingora_proxy::Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> PingoraResult<()> {
        // 委托给内部的 PortProxy 实现
        self.inner
            .response_filter(session, upstream_response, ctx)
            .await
    }
}

/// 便捷函数：快速启动 Pingora 代理服务器
///
/// 注意：此函数启动后会阻塞直到进程退出，因为内部创建的 shutdown 通道
/// 的 sender 会在函数结束时立即 drop，导致 `start()` 立即返回。
/// 如需长时间运行，请使用 `PingoraServerManager::new()` + `start(shutdown_rx)` 组合。
pub async fn start_pingora_proxy(config: ProxyConfig) -> Result<(), ProxyError> {
    let (_shutdown_tx, shutdown_rx) = oneshot::channel();
    let mut manager = PingoraServerManager::new(config);
    manager.start(shutdown_rx).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::{RemoteWakeState, WakeOutcome};
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct RunningWakeControl;

    #[async_trait::async_trait]
    impl shared_types::AppWakeControl for RunningWakeControl {
        fn is_stopped(&self, _app_id: &str) -> bool {
            false
        }

        async fn ensure_running(&self, _app_id: &str) -> WakeOutcome {
            WakeOutcome::AlreadyRunning
        }

        async fn remote_wake_pending(&self, _app_id: &str) -> bool {
            false
        }

        async fn remote_wake_state_fresh(&self, _app_id: &str) -> RemoteWakeState {
            RemoteWakeState::Running
        }

        fn wake_timeout(&self) -> Duration {
            Duration::from_secs(4)
        }
    }

    #[test]
    fn test_server_manager_creation() {
        let config = ProxyConfig::default();
        let manager = PingoraServerManager::new(config);

        // 测试创建管理器
        assert_eq!(manager.config.listen_port, 8080);
        assert_eq!(manager.config.default_backend_port, 3000);
    }

    #[tokio::test]
    async fn test_start_stop_server() {
        let _manager = PingoraServerManager::new(ProxyConfig::with_listen_port(8081));

        // 测试启动和停止（在测试中可能需要更复杂的逻辑）
        // 这里只是验证方法调用不 panic
        // 在实际测试中需要更完善的设置
    }

    /// The first real proxy dial sees ConnectionRefused. The same POST must
    /// wait for the port to open and reach the backend exactly once.
    #[tokio::test]
    async fn prod_post_waits_for_service_connection_without_replaying_body() {
        let backend_reservation =
            std::net::TcpListener::bind("127.0.0.1:0").expect("reserve backend port");
        let backend_addr = backend_reservation.local_addr().expect("backend address");
        drop(backend_reservation);
        #[cfg(feature = "deploy-host")]
        let backend_host = {
            let host = format!("coldcase-backend-{}", backend_addr.port());
            shared_types::published::register_port(&host, backend_addr.port(), backend_addr.port());
            host
        };
        #[cfg(not(feature = "deploy-host"))]
        let backend_host = "127.0.0.1".to_string();
        let proxy_reservation =
            std::net::TcpListener::bind("127.0.0.1:0").expect("reserve proxy port");
        let proxy_port = proxy_reservation
            .local_addr()
            .expect("proxy address")
            .port();
        drop(proxy_reservation);

        let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
            .with_wake_control(Arc::new(RunningWakeControl));
        manager
            .service
            .add_app_backend("coldcase", backend_addr.port(), backend_host.clone());
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        drop(shutdown_tx);
        manager.start(shutdown_rx).await.expect("start local proxy");

        let mut downstream = None;
        for _ in 0..100 {
            if let Ok(stream) = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port)).await {
                downstream = Some(stream);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut downstream = downstream.expect("proxy listener became ready");
        let backend = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(450)).await;
            let listener = tokio::net::TcpListener::bind(backend_addr)
                .await
                .expect("start delayed backend");
            let mut business_requests = 0;
            loop {
                let (mut socket, _) = listener.accept().await.expect("accept proxy connection");
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    match tokio::time::timeout(Duration::from_secs(2), socket.read(&mut chunk))
                        .await
                    {
                        Ok(Ok(0)) => break, // TCP readiness probe, no business request
                        Ok(Ok(read)) => {
                            request.extend_from_slice(&chunk[..read]);
                            if request.windows(4).any(|part| part == b"\r\n\r\n")
                                && request.ends_with(b"abc")
                            {
                                break;
                            }
                        }
                        _ => panic!("upstream request did not finish"),
                    }
                }
                if request.is_empty() {
                    continue;
                }
                assert!(request.starts_with(b"POST / HTTP/1.1"));
                assert!(request.ends_with(b"abc"));
                business_requests += 1;
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await
                    .expect("reply to request");
                return business_requests;
            }
        });

        let started = Instant::now();
        downstream.write_all(
            b"POST /api/v1/userapp/proxy/app/prod/u1/coldcase/ HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
        ).await.expect("send first request");
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(6),
            downstream.read_to_end(&mut response),
        )
        .await
        .expect("proxy response deadline")
        .expect("read proxy response");
        assert!(
            response.starts_with(b"HTTP/1.1 200"),
            "{}",
            String::from_utf8_lossy(&response)
        );
        assert!(
            started.elapsed() >= Duration::from_millis(350),
            "request bypassed delayed backend"
        );
        assert_eq!(backend.await.expect("backend task"), 1);

        // The same endpoint remains unavailable: the request must stop at its
        // own deadline and tell the caller to retry, without replaying a POST.
        let mut unavailable = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port))
            .await
            .expect("connect to proxy again");
        let started = Instant::now();
        unavailable
            .write_all(
                b"POST /api/v1/userapp/proxy/app/prod/u1/coldcase/ HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\nConnection: close\r\n\r\nxyz",
            )
            .await
            .expect("send unavailable request");
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(6),
            unavailable.read_to_end(&mut response),
        )
        .await
        .expect("unavailable response deadline")
        .expect("read unavailable response");
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 503"), "{text}");
        assert!(
            text.to_ascii_lowercase().contains("retry-after: 15"),
            "{text}"
        );
        assert!(started.elapsed() >= Duration::from_secs(3));
        #[cfg(feature = "deploy-host")]
        shared_types::published::unregister(&backend_host);
    }
}
