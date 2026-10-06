//! HTTP 服务器启动模块
//!
//! 提供便捷的 HTTP 服务器启动 API
//! 支持 HTTP REST API 和可选的 Pingora 代理服务

use anyhow::{Context, Result};
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
    /// 跨协议共享的 project_id → service_uuid 映射（gRPC 与 HTTP 双开时必须
    /// 注入同一实例——两份独立 map 互不可见：经 gRPC 发起的 StopAgent 找不到
    /// HTTP 域写入的映射，shared_api_key_manager 中该 uuid 的 api_key 永不被
    /// 清理，进程内敏感配置累积）。None = 自建（单协议形态，无跨协议清理需求）。
    pub project_uuid_map: Option<Arc<dashmap::DashMap<String, String>>>,
    /// P0-1: Agent 管理注册表(可选,启用 /agent-mgmt/* 路由)
    pub agent_mgmt_registry: Option<Arc<crate::agent_mgmt::AgentRegistry>>,
    /// P0-1: Agent 安装目录管理(可选,启用 /agent-mgmt/* 路由)
    pub agent_mgmt_path_manager: Option<crate::agent_mgmt::PathManager>,
}

/// HTTP 服务器控制柄
///
/// 用于控制 HTTP 服务器的生命周期
#[derive(Clone)]
pub struct HttpServerHandle {
    /// 关闭信号令牌
    shutdown_token: CancellationToken,
    /// 活跃任务集合
    join_set: Arc<tokio::sync::Mutex<JoinSet<Result<()>>>>,
    /// 所有控制柄共享关闭顺序、总预算及已确认的失败结果。
    shutdown_state: Arc<tokio::sync::Mutex<HttpShutdownState>>,
    /// Pingora 结果（用于调用 stop）
    #[cfg(feature = "proxy")]
    pingora_result: Arc<tokio::sync::Mutex<Option<crate::proxy_agent::PingoraStartResult>>>,
}

#[derive(Default)]
struct HttpShutdownState {
    deadline: Option<tokio::time::Instant>,
    grace_deadline: Option<tokio::time::Instant>,
    http_abort_requested: bool,
    http_failures: Vec<String>,
    proxy_error: Option<String>,
    error: Option<String>,
}

impl HttpServerHandle {
    /// 检查是否收到关闭信号
    pub fn is_shutdown(&self) -> bool {
        self.shutdown_token.is_cancelled()
    }

    /// 停止 HTTP 服务器并等待所有任务完成
    pub async fn stop(&self) -> Result<()> {
        // clone 之间串行确认；取消任一调用后，下次调用继续同一关闭。
        let mut shutdown_state = self.shutdown_state.lock().await;
        if let Some(error) = &shutdown_state.error {
            anyhow::bail!("{error}");
        }
        let started = tokio::time::Instant::now();
        let deadline = *shutdown_state
            .deadline
            .get_or_insert(started + Duration::from_secs(10));
        let grace_deadline = *shutdown_state
            .grace_deadline
            .get_or_insert(deadline.min(started + Duration::from_secs(3)));
        info!("Stopping HTTP server...");

        // 1. 发送关闭信号
        self.shutdown_token.cancel();

        // 2. 两个服务同时排空，避免等待代理时延长 HTTP 的三秒宽限。
        let HttpShutdownState {
            http_abort_requested,
            http_failures,
            proxy_error,
            ..
        } = &mut *shutdown_state;
        let proxy_shutdown = async {
            let result: Result<()> = async {
                #[cfg(feature = "proxy")]
                {
                    // 所有权始终留在共享对象；取消等待只释放 guard，不 detach 任务。
                    let mut pingora_guard = self.pingora_result.lock().await;
                    if let Some(pingora) = pingora_guard.as_mut() {
                        pingora
                            .stop_until(deadline)
                            .await
                            .context("Pingora shutdown failed")?;
                        // 仅真实代理和健康任务均成功退出后才移除对象。
                        drop(pingora_guard.take());
                    }
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                error!(%error, "Pingora shutdown failed; exit remains unconfirmed");
                // join! 的另一分支仍可能等待；失败须在下次 await 前持久化。
                *proxy_error = Some(format!("{error:#}"));
            }
        };
        tokio::join!(
            proxy_shutdown,
            self.join_http_until(
                grace_deadline,
                deadline,
                http_abort_requested,
                http_failures
            )
        );
        let mut failures = Vec::new();
        if let Some(error) = &shutdown_state.proxy_error {
            failures.push(error.clone());
        }
        failures.extend(shutdown_state.http_failures.iter().cloned());
        if !failures.is_empty() {
            let error = failures.join("; ");
            error!(%error, "HTTP server shutdown failed");
            shutdown_state.error = Some(error.clone());
            anyhow::bail!("{error}");
        }
        info!("HTTP server stopped");
        Ok(())
    }

    async fn join_http_until(
        &self,
        grace_deadline: tokio::time::Instant,
        deadline: tokio::time::Instant,
        abort_requested: &mut bool,
        failures: &mut Vec<String>,
    ) {
        // 3. HTTP 与 Pingora 在同一时刻收到关闭，保留三秒连接排空宽限。
        let mut join_set = self.join_set.lock().await;
        if !*abort_requested {
            loop {
                match tokio::time::timeout_at(grace_deadline, join_set.join_next()).await {
                    Ok(Some(Ok(Ok(())))) => {
                        info!("Task exited normally");
                    }
                    Ok(Some(Ok(Err(error)))) => {
                        // join_next 已消费结果，须同步写回共享状态以抵抗外层取消。
                        failures.push(format!("HTTP service failed: {error:#}"));
                    }
                    Ok(Some(Err(error))) => {
                        failures.push(format!("HTTP task failed: {error}"));
                    }
                    Ok(None) => break,
                    Err(_) => {
                        warn!(
                            "HTTP graceful shutdown exceeded 3 seconds, cancelling remaining tasks"
                        );
                        *abort_requested = true;
                        join_set.abort_all();
                        break;
                    }
                }
            }
        }

        // abort 仅请求取消，必须在父 deadline 内实际 drain，才确认任务退出。
        while !join_set.is_empty() {
            match tokio::time::timeout_at(deadline, join_set.join_next()).await {
                Ok(Some(Ok(Ok(())))) => {}
                Ok(Some(Ok(Err(error)))) => {
                    failures.push(format!("HTTP service failed during shutdown: {error:#}"));
                }
                Ok(Some(Err(error))) if error.is_cancelled() => {}
                Ok(Some(Err(error))) => {
                    failures.push(format!("HTTP task failed during shutdown: {error}"));
                }
                Ok(None) => break,
                Err(_) => {
                    failures.push(
                        "HTTP shutdown deadline expired; task exit remains unconfirmed".into(),
                    );
                    break;
                }
            }
        }
    }
}

/// 启动 HTTP 服务器
///
/// # 示例
///
/// ```no_run
/// use agent_runner::{AgentSessionService, start_http_server, HttpServerConfig, AppConfig, ProxyConfig};
/// use std::sync::Arc;
/// use std::path::PathBuf;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     // 创建 Agent Session Service（第二参为 ACP session 创建超时秒数，取自 GrpcTimeoutConfig）
///     let agent_session_service = Arc::new(AgentSessionService::new(
///         agent_abstraction::launcher::direct_model_runtime_env_resolver(),
///         100,
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
///                 // health_check 详 HealthCheckConfig（agent_runner::config）文档
///                 ..Default::default()
///             }),
///             ..Default::default()
///         },
///         agent_session_service,
///         shared_api_key_manager: Arc::new(dashmap::DashMap::new()),
///         project_uuid_map: None,       // 单 HTTP 形态自建；gRPC 双开时注入共享实例
///         agent_mgmt_registry: None,    // P0-1: 不启用 /agent-mgmt/* 路由
///         agent_mgmt_path_manager: None,
///     };
///
///     // 启动 HTTP Server
///     let handle = start_http_server(config).await?;
///
///     // 优雅停止
///     handle.stop().await?;
///     Ok(())
/// }
/// ```
pub async fn start_http_server(config: HttpServerConfig) -> Result<HttpServerHandle> {
    // 设置 mcp-proxy 日志目录（如果配置了的话）
    // 使用 OnceLock 替代 env::set_var，避免多线程环境下的 UB（Rust 1.84+）
    if let Some(ref log_dir) = config.app_config.mcp_proxy_log_dir {
        agent_abstraction::launcher::set_mcp_proxy_log_dir(log_dir.clone());
        info!("Set MCP_PROXY_LOG_DIR={}", log_dir);
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
    let mut state = AppState::new(
        config.app_config.clone(),
        config.agent_session_service,
        config.shared_api_key_manager,
        config.project_uuid_map,
    );

    // 2.5 P0-1: 启用 agent_mgmt 路由(若提供)
    if let (Some(registry), Some(pm)) = (
        config.agent_mgmt_registry.clone(),
        config.agent_mgmt_path_manager.clone(),
    ) {
        state = state.with_agent_mgmt(registry, pm);
        info!("P0-1: agent-mgmt HTTP routes enabled");
    } else {
        info!("P0-1: agent-mgmt HTTP routes disabled (no registry/path_manager)");
    }

    let state = Arc::new(state);

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

        server.await.context("HTTP service exited with an error")?;
        info!("HTTP service exited normally");
        Ok(())
    });

    // 创建 handle
    let handle = HttpServerHandle {
        shutdown_token,
        join_set,
        shutdown_state: Arc::new(tokio::sync::Mutex::new(HttpShutdownState::default())),
        #[cfg(feature = "proxy")]
        pingora_result,
    };

    Ok(handle)
}

#[cfg(all(test, unix))]
mod shutdown_tests;
