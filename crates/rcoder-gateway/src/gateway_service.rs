//! Pingora 服务器生命周期管理
//!
//! 封装 Pingora Server 创建、TCP 监听、线程启动和关闭信号处理。

use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::info;

use pingora_core::server::Server;
use pingora_core::server::configuration::Opt;
use pingora_proxy::http_proxy_service;

use crate::config::GatewayConfig;
use crate::gateway_proxy::GatewayProxy;

/// Gateway 服务管理器
pub struct GatewayService {
    config: Arc<GatewayConfig>,
}

impl GatewayService {
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// 启动 Pingora 代理服务器
    ///
    /// 在独立线程中运行 `run_forever()`，通过 `shutdown_rx` 接收关闭信号。
    pub async fn start(&self, shutdown_rx: oneshot::Receiver<()>) -> anyhow::Result<()> {
        info!(
            "[GATEWAY] starting on 0.0.0.0:{} → control={}",
            self.config.gateway_port, self.config.control_plane_url
        );

        let opt = Opt::default();
        let mut server = Server::new(Some(opt))
            .map_err(|e| anyhow::anyhow!("Failed to create Pingora server: {}", e))?;
        server.bootstrap();

        let proxy = GatewayProxy::new(self.config.clone());
        let mut http_service = http_proxy_service(&server.configuration, proxy);

        http_service.add_tcp(&format!("0.0.0.0:{}", self.config.gateway_port));
        server.add_service(http_service);

        let gateway_port = self.config.gateway_port;
        std::thread::spawn(move || {
            info!("[GATEWAY] Pingora server running on port {}", gateway_port);
            // run_forever() 内部监听 SIGTERM，收到后执行优雅关闭，
            // 最终调用 process::exit(0) 终止整个进程。
            server.run_forever();
        });

        // 等待外部关闭信号（tokio ctrlc handler 或 K8s SIGTERM）
        // 注意：Pingora 线程收到 SIGTERM 后会自行执行 process::exit(0)，
        // 通常比 shutdown_rx 更先触发进程退出。
        let _ = shutdown_rx.await;
        info!("[GATEWAY] shutdown signal received");
        Ok(())
    }
}
