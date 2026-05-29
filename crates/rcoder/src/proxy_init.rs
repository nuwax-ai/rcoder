//! Pingora 反向代理初始化

use std::sync::Arc;

use arc_swap::ArcSwap;
use rcoder_proxy::{PingoraServerManager, ProxyConfig};
use tracing::{error, info};

use crate::config::AppConfig;

pub struct ProxyInitResult {
    pub proxy_handle: Option<tokio::task::JoinHandle<()>>,
    pub pingora_service: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    pub pingora_shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

pub async fn init_proxy(
    config: &AppConfig,
    api_key_config: Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>,
) -> ProxyInitResult {
    let Some(proxy_config) = &config.proxy_config else {
        info!("[Pingora] proxy_config not configured, skipping Pingora startup");
        return ProxyInitResult {
            proxy_handle: None,
            pingora_service: None,
            pingora_shutdown_tx: None,
        };
    };

    info!(
        "Starting Pingora reverse proxy service, listening on port: {}",
        proxy_config.listen_port
    );
    info!(
        "Proxy route format: /proxy/{{port}}{{/path}} - e.g.: /proxy/{}/health",
        config.port
    );

    info!("[Pingora] starting to initialize Pingora config...");
    info!("🔧 [Pingora] listen port: {}", proxy_config.listen_port);
    info!(
        "🔧 [Pingora] Default backend port: {}",
        proxy_config.default_backend_port
    );
    info!("🔧 [Pingora] backend host: {}", proxy_config.backend_host);

    let pingora_config = ProxyConfig {
        listen_port: proxy_config.listen_port,
        default_backend_port: proxy_config.default_backend_port,
        backend_host: proxy_config.backend_host.clone(),
        port_param: proxy_config.port_param.clone(),
        config_file: None,
        verbose: false,
        request_timeout_seconds: Some(proxy_config.http_client.request_timeout_seconds),
        connect_timeout_seconds: Some(proxy_config.http_client.connect_timeout_seconds),
        pool_idle_timeout_seconds: Some(proxy_config.http_client.pool_idle_timeout_seconds),
    };

    info!("[Pingora] Pingora config created successfully");

    info!("[Pingora] PingoraServerManager created successfully");
    let mut server_manager = PingoraServerManager::new(pingora_config)
        .with_api_key_config(Arc::clone(&api_key_config));
    let pingora_service = server_manager.service();
    info!("[Pingora] API Key config already loaded (no updates)");

    if proxy_config.health_check.enabled {
        let hc = &proxy_config.health_check;
        info!(
            "🔧 [Pingora] Starting health check loop: interval={}s, timeout={}s",
            hc.interval_seconds, hc.timeout_seconds
        );
        pingora_service.start_health_check_loop(hc.interval_seconds, hc.timeout_seconds * 1000);
        info!("[Pingora] health check already started");
    }

    info!("[Pingora] starting Pingora server...");
    let (pingora_shutdown_tx, pingora_shutdown_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        info!("📍 [Pingora] calling server_manager.start()...");
        if let Err(e) = server_manager.start(pingora_shutdown_rx).await {
            error!("[Pingora] Pingora proxy start failed, error: {:?}", e);
            std::process::exit(1);
        }
        info!("[Pingora] server started");
    });

    info!("[Pingora] already started");

    ProxyInitResult {
        proxy_handle: Some(handle),
        pingora_service: Some(pingora_service),
        pingora_shutdown_tx: Some(pingora_shutdown_tx),
    }
}

pub fn log_proxy_info(config: &AppConfig) {
    if let Some(proxy_config) = &config.proxy_config {
        info!("Pingora proxy already started");
        info!("📡 listenport: {}", proxy_config.listen_port);
        info!("route: /proxy/{{port}}{{/path}} - example: /proxy/3000/api/users");
        info!("format: query port parameter to proxy request");
        info!("💡 example:");
        info!(
            "   http://localhost:{}/proxy/{}/health → http://127.0.0.1:{}/health",
            proxy_config.listen_port, config.port, config.port
        );
        info!(
            "   http://localhost:{}/proxy/9000/health → http://127.0.0.1:9000/health (dynamic discovery)",
            proxy_config.listen_port
        );
    }
}
