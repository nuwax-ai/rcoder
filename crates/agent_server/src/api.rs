//! API 路由模块

use crate::{
    agent::AgentManager,
    config::AgentServerConfig,
    handlers::{
        agent_cancel_handler, agent_progress_handler, agent_status_handler, agent_stop_handler,
        chat_handler, health_handler,
    },
    shutdown::ShutdownManager,
};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use http::HeaderValue;
use tracing::info;

/// API 应用状态
#[derive(Clone)]
pub struct ApiState {
    /// Agent 管理器
    pub agent_manager: Arc<AgentManager>,
    /// 配置
    pub config: AgentServerConfig,
    /// 关闭管理器
    pub shutdown_manager: ShutdownManager,
}

/// 创建 API 路由
pub fn create_api_router(
    state: ApiState,
    config: &AgentServerConfig,
) -> Router {
    info!("创建 API 路由，端口: {}", config.port);

    let mut router = Router::new()
        // 健康检查 - 完全复制 rcoder 的健康检查路由
        .route("/health", get(health_handler::health_check))
        // 聊天接口
        .route("/chat", post(chat_handler::handle_chat))
        // Agent 管理接口 - 完全复制 rcoder 的路由配置
        .route(
            "/agent/progress/{session_id}",
            get(agent_progress_handler::agent_progress),
        )
        .route("/agent/session/cancel", post(agent_cancel_handler::cancel_session))
        .route("/agent/status/{project_id}", get(agent_status_handler::agent_status))
        .with_state(state);

    // 添加中间件
    if config.enable_cors {
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
            .allow_credentials(true);

        router = router.layer(cors);
    }

    // 添加追踪层
    router = router.layer(
        ServiceBuilder::new()
            .layer(TraceLayer::new_for_http())
            .into_inner(),
    );

    // 404 处理
    router = router.fallback(handler_404);

    router
}

/// 404 处理器
async fn handler_404() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": "接口未找到",
            "message": "请求的接口不存在",
            "code": 404
        })),
    )
}

/// 初始化 CORS 配置
pub fn create_cors_layer(config: &AgentServerConfig) -> CorsLayer {
    if !config.enable_cors {
        return CorsLayer::new().allow_origin(HeaderValue::from_static("http://localhost:3000"));
    }

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .allow_credentials(true)
}

/// 创建服务器地址
pub fn create_server_addr(config: &AgentServerConfig) -> std::net::SocketAddr {
    format!("0.0.0.0:{}", config.port)
        .parse()
        .expect("无效的服务器地址")
}