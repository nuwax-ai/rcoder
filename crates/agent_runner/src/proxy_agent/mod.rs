pub mod cleanup_task;

#[cfg(feature = "proxy")]
use crate::config::ProxyConfig;
#[cfg(feature = "proxy")]
use anyhow::{Context, Result};
use dashmap::DashMap;
#[cfg(feature = "proxy")]
use rcoder_proxy::{PingoraServerManager, ProxyConfig as PingoraProxyConfig};
#[cfg(feature = "proxy")]
use std::net::TcpListener;
#[cfg(feature = "proxy")]
use std::sync::Arc;
use std::sync::LazyLock;
#[cfg(feature = "proxy")]
use tracing::{error, info};

/// Pingora 启动结果
///
/// 持有关闭信号的发送端，`stop()` 时直接发送信号，无需 Mutex 锁。
#[cfg(feature = "proxy")]
pub struct PingoraStartResult {
    /// 关闭信号发送端
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(feature = "proxy")]
impl PingoraStartResult {
    /// 停止 Pingora 服务器
    pub async fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// 启动 Pingora 代理服务
///
/// 封装 Pingora 的创建和启动逻辑，供 main.rs 和 http_server/start.rs 复用。
/// shutdown 通道在外部创建，`stop()` 直接发送信号，不经过 Mutex，消除死锁风险。
#[cfg(feature = "proxy")]
pub fn start_pingora(
    proxy_config: &ProxyConfig,
    shared_api_key_manager: Arc<dashmap::DashMap<String, shared_types::ModelProviderConfig>>,
) -> Result<PingoraStartResult> {
    info!(
        "Starting Pingora reverse proxy service, listening on port: {}",
        proxy_config.listen_port
    );
    info!(
        "Proxy route format: /proxy/{{port}}{{/path}} - e.g.: /proxy/{}/health",
        proxy_config.default_backend_port
    );

    preflight_proxy_port(proxy_config.listen_port)?;

    let pingora_config = PingoraProxyConfig {
        listen_port: proxy_config.listen_port,
        default_backend_port: proxy_config.default_backend_port,
        backend_host: proxy_config.backend_host.clone(),
        port_param: proxy_config.port_param.clone(),
        config_file: None,
        verbose: false,
        ..Default::default()
    };

    // 创建 Pingora 服务器管理器
    let mut server_manager =
        PingoraServerManager::new(pingora_config).with_api_key_manager(shared_api_key_manager);

    let pingora_service = server_manager.service();

    // 启动健康检查循环（按配置）
    if proxy_config.health_check.enabled {
        let hc = &proxy_config.health_check;
        pingora_service.start_health_check_loop(hc.interval_seconds, hc.timeout_seconds * 1000);
    }

    // 在外部创建 shutdown 通道，避免通过 Mutex 发送信号导致死锁
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    // 在后台任务中启动 Pingora（直接 move server_manager，无需 Arc<Mutex<>>）
    tokio::spawn(async move {
        if let Err(e) = server_manager.start(shutdown_rx).await {
            error!("Failed to start Pingora proxy server: {}", e);
        }
    });

    info!(
        "✅ Pingora 代理服务已启动在端口 {}",
        proxy_config.listen_port
    );

    Ok(PingoraStartResult {
        shutdown_tx: Some(shutdown_tx),
    })
}

#[cfg(feature = "proxy")]
fn preflight_proxy_port(listen_port: u16) -> Result<()> {
    let addr = format!("0.0.0.0:{listen_port}");
    let listener = TcpListener::bind(&addr)
        .with_context(|| format!("Pingora proxy port preflight failed: {addr} is unavailable"))?;
    drop(listener);
    info!("Pingora proxy port preflight passed: {}", addr);
    Ok(())
}

/// 会话级别的 request_id 上下文映射（project_id -> request_id）
/// 用于在 session_notification 回调中获取当前请求的 request_id
/// 避免使用 PROJECT_AND_AGENT_INFO_MAP 导致的锁竞争问题
/// 注意：使用 project_id 而非 session_id，确保同一项目的多次请求能自动覆盖为最新值
pub static SESSION_REQUEST_CONTEXT: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);
