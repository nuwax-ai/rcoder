//! HTTP 路由定义
//!
//! 定义所有 HTTP 端点和路由

use axum::{
    Json, Router,
    routing::{get, post},
};
use dashmap::DashMap;
use std::sync::Arc;
use tower_http::limit::RequestBodyLimitLayer;
use utoipa::{OpenApi, ToSchema};
use utoipa_swagger_ui::SwaggerUi;

use crate::agent_runtime::AgentRuntime;
use crate::api_key_manager::ApiKeyManager;
use crate::config::AppConfig;

/// HTTP 应用状态
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub agent_runtime: Arc<AgentRuntime>,
    pub api_key_manager: Arc<ApiKeyManager>,
    pub shared_api_key_manager: Arc<DashMap<String, shared_types::ModelProviderConfig>>,
    pub project_uuid_map: Arc<DashMap<String, String>>,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        agent_runtime: Arc<AgentRuntime>,
        shared_api_key_manager: Arc<DashMap<String, shared_types::ModelProviderConfig>>,
    ) -> Self {
        Self {
            config,
            agent_runtime: agent_runtime.clone(),
            api_key_manager: Arc::new(ApiKeyManager::from_shared(shared_api_key_manager.clone())),
            shared_api_key_manager,
            project_uuid_map: Arc::new(DashMap::new()),
        }
    }
}

/// 创建 Computer Agent 路由
pub fn create_router(state: Arc<AppState>) -> Router {
    use super::handlers::{
        computer_cancel, computer_chat, computer_progress, computer_status, computer_stop,
    };

    let computer_routes = Router::new()
        .route("/computer/chat", post(computer_chat::handle_computer_chat))
        .route(
            "/computer/agent/stop",
            post(computer_stop::handle_computer_stop),
        )
        .route(
            "/computer/agent/status",
            post(computer_status::handle_computer_status),
        )
        .route(
            "/computer/agent/session/cancel",
            post(computer_cancel::handle_computer_cancel),
        )
        .route(
            "/computer/progress/{session_id}",
            get(computer_progress::handle_computer_progress),
        )
        .with_state(state.clone());

    // 通用路由
    let api_routes = Router::new()
        .route("/health", get(health_check))
        .with_state(state.clone());

    // 组合路由
    Router::new()
        .merge(computer_routes)
        .merge(api_routes)
        .merge(create_swagger_ui())
        .layer(RequestBodyLimitLayer::new(50 * 1024 * 1024)) // 🔥 50MB body 限制
}

/// 健康检查端点
///
/// 检查服务的健康状态
#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "服务健康状态", body = shared_types::HealthResponse)
    ),
    tag = "system"
)]
pub async fn health_check() -> Json<shared_types::HealthResponse> {
    Json(shared_types::HealthResponse::new("agent-runner"))
}

/// 创建 Swagger UI
fn create_swagger_ui() -> SwaggerUi {
    use super::handlers::{
        computer_cancel::__path_handle_computer_cancel, computer_chat::__path_handle_computer_chat,
        computer_progress::__path_handle_computer_progress,
        computer_status::__path_handle_computer_status, computer_stop::__path_handle_computer_stop,
    };

    #[derive(OpenApi)]
    #[openapi(
        paths(
            handle_computer_chat,
            handle_computer_status,
            handle_computer_stop,
            handle_computer_cancel,
            handle_computer_progress,
            health_check,
        ),
        components(schemas(
            shared_types::ComputerChatRequest,
            shared_types::ChatResponse,
            shared_types::ComputerAgentStatusRequest,
            shared_types::ComputerAgentStatusResponse,
            shared_types::ComputerAgentStopRequest,
            shared_types::ComputerAgentStopResponse,
            shared_types::ComputerAgentCancelRequest,
            shared_types::ComputerAgentCancelResponse,
            shared_types::HealthResponse,
        )),
        tags(
            (name = "Computer Agent", description = "Computer Agent HTTP API"),
            (name = "System", description = "系统管理接口")
        )
    )]
    struct ApiDoc;

    SwaggerUi::new("/api/docs").url("/api-docs/openapi.json", ApiDoc::openapi())
}
