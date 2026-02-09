//! HTTP 服务器启动模块
//!
//! 提供便捷的 HTTP 服务器启动 API
//! 支持 HTTP REST API 和可选的 Pingora 代理服务

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use crate::agent_runtime::AgentRuntime;
use crate::config::AppConfig;
use crate::http_server::router::{AppState, create_router};
use crate::proxy_agent::cleanup_task::{start_cleanup_task, CleanupConfig};
use crate::proxy_agent::set_unlimited_mode;
#[cfg(feature = "proxy")]
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
    /// 关闭信号令牌
    shutdown_token: CancellationToken,
    /// 活跃任务集合
    join_set: Arc<tokio::sync::Mutex<JoinSet<()>>>,
    /// Pingora 结果（用于调用 stop）
    #[cfg(feature = "proxy")]
    pingora_result: Arc<tokio::sync::Mutex<Option<crate::proxy_agent::PingoraStartResult>>>,
}

impl HttpServerHandle {
    /// 检查是否收到关闭信号
    pub fn is_shutdown(&self) -> bool {
        self.shutdown_token.is_cancelled()
    }

    /// 停止 HTTP 服务器并等待所有任务完成
    pub async fn stop(&self) {
        info!("正在停止 HTTP 服务器...");

        // 1. 发送关闭信号
        self.shutdown_token.cancel();

        // 2. 停止 Pingora 服务
        #[cfg(feature = "proxy")]
        {
            let mut pingora_guard = self.pingora_result.lock().await;
            if let Some(mut pingora) = pingora_guard.take() {
                pingora.stop().await;
            }
        }

        // 3. 等待所有任务完成（带超时）
        let timeout = Duration::from_secs(30);
        let mut join_set = self.join_set.lock().await;

        loop {
            match tokio::time::timeout(timeout, join_set.join_next()).await {
                Ok(Some(Ok(()))) => {
                    info!("任务正常结束");
                }
                Ok(Some(Err(e))) => {
                    warn!("任务出错: {:?}", e);
                }
                Ok(None) => {
                    // JoinSet 为空，所有任务已完成
                    break;
                }
                Err(_) => {
                    warn!("等待任务完成超时");
                    break;
                }
            }
        }

        info!("HTTP 服务器已停止");
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
    // 设置无限制模式（HTTP Server 部署不限制槽位）
    set_unlimited_mode(true);

    // 创建关闭信号令牌
    let shutdown_token = CancellationToken::new();
    let join_set = Arc::new(tokio::sync::Mutex::new(JoinSet::new()));
    #[cfg(feature = "proxy")]
    let pingora_result = Arc::new(tokio::sync::Mutex::new(None));

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
    let cleanup_token = shutdown_token.child_token();
    join_set.lock().await.spawn(async move {
        tokio::select! {
            _ = start_cleanup_task(cleanup_config) => {}
            _ = cleanup_token.cancelled() => {
                info!("清理任务收到关闭信号");
            }
        }
    });

    // 2. 启动 Pingora 代理服务（如果配置了且启用了 proxy feature）
    #[cfg(feature = "proxy")]
    if let Some(proxy_config) = &config.app_config.proxy_config {
        let result = start_pingora(proxy_config, config.shared_api_key_manager.clone());
        // 保存 Pingora 结果以便后续调用 stop
        *pingora_result.lock().await = Some(result);
    } else {
        info!("Pingora 代理服务未配置，跳过启动");
    }

    #[cfg(not(feature = "proxy"))]
    info!("Pingora 代理服务未启用 (proxy feature 未开启)");

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

    // 6. 启动 HTTP 服务任务
    let http_token = shutdown_token.child_token();
    // 将 listener 和 app 移入任务中
    let http_app = app;
    let http_listener = listener;
    join_set.lock().await.spawn(async move {
        // 使用 graceful shutdown wrapper
        let server = axum::serve(http_listener, http_app)
            .with_graceful_shutdown(async move {
                let _ = http_token.cancelled().await;
            });

        match server.await {
            Ok(()) => info!("HTTP 服务正常退出"),
            Err(e) => error!("HTTP 服务错误: {:?}", e),
        }
    });

    // 创建 handle
    let handle = HttpServerHandle {
        shutdown_token,
        join_set,
        #[cfg(feature = "proxy")]
        pingora_result,
    };

    Ok(handle)
}
