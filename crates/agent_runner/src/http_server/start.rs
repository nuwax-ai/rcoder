//! HTTP 服务器启动模块
//!
//! 提供便捷的 HTTP 服务器启动 API

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::{error, info};

use crate::agent_runtime::AgentRuntime;
use crate::config::AppConfig;
use crate::http_server::router::{AppState, create_router};

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

/// 启动 HTTP 服务器
///
/// # 示例
///
/// ```no_run
/// use agent_runner::{AgentRuntime, start_http_server, HttpServerConfig, AppConfig};
/// use std::sync::Arc;
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
///         app_config: AppConfig::default(),
///         agent_runtime: runtime,
///         shared_api_key_manager: Arc::new(dashmap::DashMap::new()),
///     };
///
///     // 启动 HTTP Server
///     start_http_server(config).await.unwrap();
/// }
/// ```
pub async fn start_http_server(config: HttpServerConfig) -> Result<()> {
    // 创建应用状态
    let state = Arc::new(AppState::new(
        config.app_config,
        config.agent_runtime,
        config.shared_api_key_manager,
    ));

    // 创建路由
    let app = create_router(state);

    // 绑定地址
    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!("🚀 HTTP 服务器启动在端口 {}", config.port);
    info!("📄 API 文档: http://localhost:{}/api/docs", config.port);

    // 启动服务器
    axum::serve(listener, app).await.map_err(|e| {
        error!("❌ HTTP 服务器错误: {}", e);
        anyhow::anyhow!("HTTP 服务器错误: {}", e)
    })?;

    Ok(())
}
