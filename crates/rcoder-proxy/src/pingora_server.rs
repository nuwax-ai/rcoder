//! Pingora 服务器启动和管理模块
//!
//! 提供基于 Pingora 库的完整反向代理服务器启动功能，支持 HTTP/1.1 和 HTTP/2。

use anyhow::Result;
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
    /// * `config` - 共享的 Arc<ArcSwap<ApiKeyAuthConfig>>
    pub fn with_api_key_config(
        mut self,
        config: Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>,
    ) -> Self {
        let new_service = (*self.service).clone().with_api_key_config(config);
        self.service = Arc::new(new_service);
        self
    }

    /// 启动 Pingora 服务器
    ///
    /// 接受一个 `shutdown_rx` 用于接收外部关闭信号。
    /// 当 `shutdown_rx` 收到信号（或 sender 被 drop）时，`start()` 返回。
    /// Pingora 服务器线程运行 `run_forever()`，由进程退出时 OS 清理。
    pub async fn start(&mut self, shutdown_rx: oneshot::Receiver<()>) -> Result<()> {
        info!("started Pingora message proxy message ...");
        info!("📡 listen message : 0.0.0.0:{}", self.config.listen_port);
        info!("route message : /proxy/{{port}}{{/path}}");

        // 创建 Pingora 服务器配置
        let opt = Opt::default();

        // 创建 Pingora 服务器
        let mut my_server = Server::new(Some(opt))?;
        my_server.bootstrap();

        // 创建代理服务实例
        let proxy_service = self.service.create_pingora_proxy().map_err(|e| {
            error!("[PINGORA] createdproxy message failed: {}", e);
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
        info!("🔧 message created Pingora message ...");
        let server_thread = std::thread::spawn(move || {
            info!("🎯 Pingora message starting message ...");
            my_server.run_forever();
        });
        info!("Pingora message alreadycreated");

        // 等待外部关闭信号（sender 被 drop 或显式发送信号都会触发）
        let _ = shutdown_rx.await;
        info!("📴 message closed message, Pingora message OS cleanup");

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
impl pingora_proxy::ProxyHttp for ProxyServiceWrapper {
    type CTX = crate::service::TrackingCtx;

    fn new_ctx(&self) -> Self::CTX {
        crate::service::TrackingCtx::new()
    }

    async fn upstream_peer(
        &self,
        session: &mut pingora_proxy::Session,
        _ctx: &mut Self::CTX,
    ) -> PingoraResult<Box<HttpPeer>> {
        // 委托给内部的 PortProxy 实现
        self.inner.upstream_peer(session, _ctx).await
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
        upstream_response: &mut pingora_http::ResponseHeader,
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
pub async fn start_pingora_proxy(config: ProxyConfig) -> Result<()> {
    let (_shutdown_tx, shutdown_rx) = oneshot::channel();
    let mut manager = PingoraServerManager::new(config);
    manager.start(shutdown_rx).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
