//! HTTP 服务器启动模块
//!
//! 提供便捷的 HTTP 服务器启动 API
//! 支持 HTTP REST API 和可选的 Pingora 代理服务

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::agent_runtime::AgentRuntime;
use crate::config::AppConfig;
use crate::http_server::router::{AppState, create_router};
use crate::proxy_agent::cleanup_task::{start_cleanup_task, CleanupConfig};
use crate::proxy_agent::start_pingora;

/// HTTP 服务器配置
pub struct HttpServerConfig {
    /// HTTP 监听端口
    pub port: u16,
    /// 应用配置
    pub app_config: AppConfig,
    /// Agent 运行时
    pub agent_runtime: Arc<AgentRuntime>,
    /// 共享 API Key Manager
    pub shared_api_key_manager: Arc<dashmap::DashMap<String, shared_types::ModelProviderConfig>>,
}

/// HTTP 服务器控制柄
///
/// 用于控制 HTTP 服务器的生命周期
#[derive(Clone)]
pub struct HttpServerHandle {
    /// HTTP 服务关闭标志 (使用 AtomicBool 以支持 Clone)
    http_shutdown: Arc<AtomicBool>,
    /// Pingora 服务关闭标志 (使用 AtomicBool 以支持 Clone)
    pingora_shutdown: Arc<AtomicBool>,
}

impl HttpServerHandle {
    /// 停止 HTTP 服务器
    pub async fn stop(&self) {
        info!("正在停止 HTTP 服务器...");

        // 发送 HTTP 关闭信号
        self.http_shutdown.store(true, Ordering::SeqCst);

        // 发送 Pingora 关闭信号
        self.pingora_shutdown.store(true, Ordering::SeqCst);

        info!("HTTP 服务器停止信号已发送");
    }
}

/// 启动 HTTP 服务器
///
/// # 示例
///
/// ```no_run
/// use agent_runner::{AgentRuntime, start_http_server, HttpServerConfig, AppConfig, ProxyConfig, HealthCheckConfig};
/// use std::sync::Arc;
/// use std::path::PathBuf;
///
/// #[tokio::main]
/// async fn main() {
///     // 创建 Agent Runtime
///     let (runtime, receiver) = AgentRuntime::new(1000);
///     let runtime = Arc::new(runtime);
///     runtime.start(receiver).await;
///
///     // 配置 HTTP Server
///     let config = HttpServerConfig {
///         port: 8080,
///         app_config: AppConfig {
///             port: 8080,
///             projects_dir: PathBuf::from("/app/computer-project-workspace"),
///             // 可选：启用 Pingora 代理服务
///             proxy_config: Some(ProxyConfig {
///                 listen_port: 8088,
///                 default_backend_port: 8080,
///                 backend_host: "127.0.0.1".to_string(),
///                 port_param: "port".to_string(),
///                 health_check: HealthCheckConfig::default(),
///             }),
///             ..Default::default()
///         },
///         agent_runtime: runtime,
///         shared_api_key_manager: Arc::new(dashmap::DashMap::new()),
///     };
///
///     // 启动 HTTP Server
///     let handle = start_http_server(config).await.unwrap();
///
///     // 优雅停止
///     handle.stop().await;
/// }
/// ```
pub async fn start_http_server(config: HttpServerConfig) -> Result<HttpServerHandle> {
    // 创建关闭通道
    let http_shutdown = Arc::new(AtomicBool::new(false));
    let pingora_shutdown = Arc::new(AtomicBool::new(false));

    // 1. 启动 Agent 清理任务
    let cleanup_config = CleanupConfig {
        idle_timeout: Duration::from_secs(
            config.app_config.agent_cleanup.clone()
                .unwrap_or_default().idle_timeout_secs
        ),
        cleanup_interval: Duration::from_secs(
            config.app_config.agent_cleanup.clone()
                .unwrap_or_default().cleanup_interval_secs
        ),
    };
    let _cleanup_handle = start_cleanup_task(cleanup_config);

    // 2. 启动 Pingora 代理服务（如果配置了）
    let pingora_result = if let Some(proxy_config) = &config.app_config.proxy_config {
        let result = start_pingora(proxy_config, config.shared_api_key_manager.clone());
        Some(result)
    } else {
        info!("Pingora 代理服务未配置，跳过启动");
        None
    };

    // 3. 创建 HTTP 应用状态
    let state = Arc::new(AppState::new(
        config.app_config.clone(),
        config.agent_runtime,
        config.shared_api_key_manager,
    ));

    // 4. 创建路由
    let app = create_router(state.clone());

    // 5. 绑定地址并启动 HTTP 服务器
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!("HTTP 服务器启动在端口 {}", config.port);

    info!("HTTP API endpoints:");
    info!("  POST /computer/chat - Computer Agent chat");
    info!("  POST /computer/agent/status - Computer Agent status");
    info!("  POST /computer/agent/stop - Computer Agent stop");
    info!("  POST /computer/agent/session/cancel - Computer Agent cancel");
    info!("  GET  /computer/progress/:session_id - SSE progress stream");
    info!("  GET  /health - Health check");
    info!("  GET  /api/docs - Swagger API documentation");

    // 6. 并行运行 HTTP 和 Pingora 服务
    let handle = HttpServerHandle {
        http_shutdown: http_shutdown.clone(),
        pingora_shutdown: pingora_shutdown.clone(),
    };

    // 用于接收关闭信号
    let http_shutdown_flag = http_shutdown.clone();
    let pingora_shutdown_flag = pingora_shutdown.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = axum::serve(listener, app) => {
                    warn!("HTTP 服务已停止");
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if http_shutdown_flag.load(Ordering::SeqCst) {
                        warn!("收到 HTTP 关闭信号");
                        break;
                    }
                }
            }
        }
    });

    tokio::spawn(async move {
        if let Some(result) = pingora_result {
            loop {
                tokio::select! {
                    _ = result.handle => {
                        warn!("Pingora 代理服务已停止");
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        if pingora_shutdown_flag.load(Ordering::SeqCst) {
                            warn!("收到 Pingora 关闭信号");
                            break;
                        }
                    }
                }
            }
        }
    });

    Ok(handle)
}
