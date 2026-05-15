//! HTTP 服务器启动模块
//!
//! 提供便捷的 HTTP 服务器启动 API
//! 支持 HTTP REST API 和可选的 Pingora 代理服务

#![allow(dead_code)]

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::http_server::router::{AppState, create_router};
#[cfg(feature = "proxy")]
use crate::proxy_agent::start_pingora;
use crate::service::AgentSessionService;

/// HTTP 服务器配置
pub struct HttpServerConfig {
    /// HTTP 监听端口
    pub port: u16,
    /// 应用配置
    pub app_config: AppConfig,
    /// Agent 会话服务
    pub agent_session_service: Arc<AgentSessionService>,
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
        info!("Stopping HTTP server...");

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
        // 使用 3 秒超时：清理任务会立即退出，axum 有 3 秒进行连接排空
        let timeout = Duration::from_secs(3);
        let mut join_set = self.join_set.lock().await;

        loop {
            match tokio::time::timeout(timeout, join_set.join_next()).await {
                Ok(Some(Ok(()))) => {
                    info!("Task exited normally");
                }
                Ok(Some(Err(e))) => {
                    warn!("Task error: {:?}", e);
                }
                Ok(None) => {
                    // JoinSet 为空，所有任务已完成
                    break;
                }
                Err(_) => {
                    warn!("Timed out waiting for tasks (3s), aborting remaining tasks");
                    join_set.abort_all();
                    break;
                }
            }
        }

        info!("HTTP server stopped");
    }
}

/// 启动 HTTP 服务器
///
/// # 示例
///
/// ```no_run
/// use agent_runner::{AgentSessionService, start_http_server, HttpServerConfig, AppConfig, ProxyConfig, HealthCheckConfig};
/// use std::sync::Arc;
/// use std::path::PathBuf;
///
/// #[tokio::main]
/// async fn main() {
///     // 创建 Agent Session Service
///     let agent_session_service = Arc::new(AgentSessionService::new(
///         agent_abstraction::launcher::direct_model_runtime_env_resolver(),
///     ));
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
///                 health_check: HealthCheckConfig {
///                     enabled: true,
///                     interval_seconds: 5,
///                     timeout_seconds: 1,
///                     healthy_threshold: 2,
///                     unhealthy_threshold: 3,
///                 },
///             }),
///             ..Default::default()
///         },
///         agent_session_service,
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
    // 设置 mcp-proxy 日志目录环境变量（如果配置了的话）
    if let Some(ref log_dir) = config.app_config.mcp_proxy_log_dir {
        // SAFETY: 在服务启动时设置环境变量是安全的，此时尚未启动多线程任务
        unsafe {
            std::env::set_var("MCP_PROXY_LOG_DIR", log_dir);
        }
        info!("🔧 Set MCP_PROXY_LOG_DIR={}", log_dir);
    }

    // 创建关闭信号令牌
    let shutdown_token = CancellationToken::new();
    let join_set = Arc::new(tokio::sync::Mutex::new(JoinSet::new()));
    #[cfg(feature = "proxy")]
    let pingora_result = Arc::new(tokio::sync::Mutex::new(None));

    // 1. 启动 Pingora 代理服务（如果配置了且启用了 proxy feature）
    #[cfg(feature = "proxy")]
    if let Some(proxy_config) = &config.app_config.proxy_config {
        let result = start_pingora(proxy_config, config.shared_api_key_manager.clone())?;
        // 保存 Pingora 结果以便后续调用 stop
        *pingora_result.lock().await = Some(result);
    } else {
        info!("Pingora proxy service is not configured, skipping startup");
    }

    #[cfg(not(feature = "proxy"))]
    info!("Pingora proxy service is disabled (proxy feature not enabled)");

    // 2. 创建 HTTP 应用状态
    let state = Arc::new(AppState::new(
        config.app_config.clone(),
        config.agent_session_service,
        config.shared_api_key_manager,
    ));

    // 3. 创建路由
    let app = create_router(state.clone());

    // 4. 绑定地址并启动 HTTP 服务器
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!("HTTP server started on port {}", config.port);

    info!("HTTP API endpoints:");
    info!("  POST /computer/chat - Computer Agent chat");
    info!("  POST /computer/agent/status - Computer Agent status");
    info!("  POST /computer/agent/stop - Computer Agent stop");
    info!("  POST /computer/agent/session/cancel - Computer Agent cancel");
    info!("  GET  /computer/progress/:session_id - SSE progress stream");
    info!("  -- RCoder Agent endpoints (new) --");
    info!("  POST /chat - RCoder Agent chat");
    info!("  GET  /agent/status/:project_id - RCoder Agent status");
    info!("  POST /agent/stop - RCoder Agent stop");
    info!("  POST /agent/session/cancel - RCoder Agent cancel");
    info!("  GET  /agent/progress/:session_id - RCoder SSE progress stream");
    info!("  -- Common endpoints --");
    info!("  GET  /health - Health check");
    info!("  GET  /api/docs - Swagger API documentation");

    // 5. 启动 HTTP 服务任务
    let http_token = shutdown_token.child_token();
    // 将 listener 和 app 移入任务中
    let http_app = app;
    let http_listener = listener;
    join_set.lock().await.spawn(async move {
        // 使用 graceful shutdown wrapper
        let server = axum::serve(http_listener, http_app).with_graceful_shutdown(async move {
            let _ = http_token.cancelled().await;
        });

        match server.await {
            Ok(()) => info!("HTTP service exited normally"),
            Err(e) => error!("HTTP service error: {:?}", e),
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
