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
use std::time::Duration;
#[cfg(feature = "proxy")]
use tracing::{error, info};

/// Pingora 启动结果
///
/// 持有关闭信号、代理和健康检查任务，只有真实退出后 stop 才返回成功。
#[cfg(feature = "proxy")]
pub struct PingoraStartResult {
    /// 关闭信号发送端
    shutdown_tx: Option<tokio::sync::oneshot::Sender<tokio::time::Instant>>,
    /// 真实代理任务的完成结果
    server_task: Option<tokio::task::JoinHandle<Result<()>>>,
    health_stop: tokio::sync::watch::Sender<bool>,
    health_task: Option<tokio::task::JoinHandle<()>>,
    /// 首次受理关闭后，后续调用沿用同一总预算。
    shutdown_deadline: Option<tokio::time::Instant>,
    /// 已完成的失败结果不能在重复 stop 时被改写成成功。
    shutdown_error: Option<String>,
}

#[cfg(feature = "proxy")]
impl PingoraStartResult {
    #[cfg(all(test, unix))]
    pub(crate) fn shutdown_observer(
        &self,
    ) -> (
        tokio::task::AbortHandle,
        tokio::sync::watch::Sender<bool>,
        Option<tokio::task::AbortHandle>,
    ) {
        (
            self.server_task
                .as_ref()
                .expect("observe original proxy task")
                .abort_handle(),
            self.health_stop.clone(),
            self.health_task
                .as_ref()
                .map(tokio::task::JoinHandle::abort_handle),
        )
    }

    /// 请求关闭并在同一个十秒预算内确认代理、健康检查任务退出。
    pub async fn stop(&mut self) -> Result<()> {
        self.stop_until(tokio::time::Instant::now() + Duration::from_secs(10))
            .await
    }

    /// 嵌入 HTTP 服务器时沿用父级 deadline，不重新分配阶段预算。
    pub(crate) async fn stop_until(&mut self, deadline: tokio::time::Instant) -> Result<()> {
        if let Some(error) = &self.shutdown_error {
            anyhow::bail!("{error}");
        }
        let deadline = *self.shutdown_deadline.get_or_insert(deadline);
        if let Some(tx) = self.shutdown_tx.take() {
            // 接收端可能已因启动失败关闭；以下实际 join 返回该失败。
            if tx.send(deadline).is_err() {
                tracing::debug!("Pingora shutdown receiver closed; observing task result");
            }
        }

        if let Some(task) = self.server_task.as_mut() {
            let completion = tokio::time::timeout_at(deadline, task).await.context(
                "Pingora proxy shutdown deadline expired; task exit remains unconfirmed",
            )?;
            drop(self.server_task.take());
            let result = completion
                .context("Pingora proxy task failed; server thread exit remains unconfirmed")
                .and_then(|result| result);
            if let Err(error) = result {
                self.shutdown_error = Some(format!("{error:#}"));
                return Err(error);
            }
        }

        // 代理已真实退出，随后才关闭健康检查；未知代理结果保持其所有权。
        self.health_stop.send_replace(true);
        if let Some(task) = self.health_task.as_mut() {
            let completion = tokio::time::timeout_at(deadline, task).await.context(
                "Pingora health check shutdown deadline expired; task exit remains unconfirmed",
            )?;
            drop(self.health_task.take());
            if let Err(error) = completion.context("Pingora health check task failed") {
                self.shutdown_error = Some(format!("{error:#}"));
                return Err(error);
            }
        }
        info!("Pingora proxy and health check tasks exited");
        Ok(())
    }
}

/// 启动 Pingora 代理服务
///
/// 封装 Pingora 的创建和启动逻辑，供 main.rs 和 http_server/start.rs 复用。
/// shutdown 通道在外部创建，`stop()` 直接发送信号，不经过 Mutex，消除死锁风险。
#[cfg(feature = "proxy")]
pub fn start_pingora(
    proxy_config: &ProxyConfig,
    shared_api_key_manager: Arc<DashMap<String, shared_types::ModelProviderConfig>>,
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

    // 生命周期对象持有关闭发送端和句柄，代理退出未确认时仍保留健康检查。
    let (health_stop, health_stopped) = tokio::sync::watch::channel(false);
    let health_task = if proxy_config.health_check.enabled {
        let hc = &proxy_config.health_check;
        Some(pingora_service.start_health_check_loop(
            hc.interval_seconds,
            hc.timeout_seconds * 1000,
            health_stopped,
        ))
    } else {
        None
    };

    // 在外部创建 shutdown 通道，避免通过 Mutex 发送信号导致死锁
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    // 在后台任务中启动 Pingora（直接 move server_manager，无需 Arc<Mutex<>>）
    let server_task = tokio::spawn(async move {
        let server_result = server_manager.start_with_deadline(shutdown_rx).await;
        if let Err(error) = &server_result {
            error!(%error, "Pingora proxy server failed");
        }
        server_result.context("Pingora proxy server failed")
    });

    info!(
        "✅ Pingora 代理服务已启动在端口 {}",
        proxy_config.listen_port
    );

    Ok(PingoraStartResult {
        shutdown_tx: Some(shutdown_tx),
        server_task: Some(server_task),
        health_stop,
        health_task,
        shutdown_deadline: None,
        shutdown_error: None,
    })
}

#[cfg(all(test, feature = "proxy", unix))]
mod lifecycle_tests;

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
