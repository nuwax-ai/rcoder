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
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::agent_runtime::AgentRuntime;
use crate::api_key_manager::ApiKeyManager;
use crate::config::AppConfig;
use crate::service::local_agent_service::LocalAgentHttpService;

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

    /// 创建 LocalAgentHttpService 实例用于 RCoder 模式
    pub fn create_local_agent_service(&self) -> Arc<LocalAgentHttpService> {
        Arc::new(LocalAgentHttpService::new(
            self.agent_runtime.clone(),
            self.shared_api_key_manager.clone(),
            self.project_uuid_map.clone(),
            self.config.projects_dir.clone(),
        ))
    }
}

/// 创建 HTTP 路由
///
/// 组合 Computer Agent 路由和 RCoder Agent 路由
pub fn create_router(state: Arc<AppState>) -> Router {
    use super::handlers::{
        computer_cancel, computer_chat, computer_progress, computer_status, computer_stop,
        rcoder_progress,
    };
    use shared_types::http_handlers;

    // 创建 LocalAgentHttpService 实例
    let local_agent_service = state.create_local_agent_service();

    // Computer Agent 路由
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

    // RCoder Agent 路由（使用 LocalAgentHttpService）
    let rcoder_routes = Router::new()
        .route("/chat", post(http_handlers::handle_chat::<LocalAgentHttpService>))
        .route(
            "/agent/session/cancel",
            post(http_handlers::handle_cancel::<LocalAgentHttpService>),
        )
        .route("/agent/stop", post(http_handlers::handle_stop::<LocalAgentHttpService>))
        .route(
            "/agent/status/{project_id}",
            get(http_handlers::handle_status::<LocalAgentHttpService>),
        )
        .route(
            "/agent/progress/{session_id}",
            get(rcoder_progress::handle_rcoder_progress),
        )
        .with_state(local_agent_service);

    // 通用路由
    let api_routes = Router::new()
        .route("/health", get(health_check))
        .with_state(state.clone());

    // 组合路由
    Router::new()
        .merge(computer_routes)
        .merge(rcoder_routes)
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
            // Computer Agent 端点
            handle_computer_chat,
            handle_computer_status,
            handle_computer_stop,
            handle_computer_cancel,
            handle_computer_progress,
            // 健康检查
            health_check,
        ),
        components(schemas(
            // Computer Agent 类型
            shared_types::ComputerChatRequest,
            shared_types::ChatResponse,
            shared_types::ComputerAgentStatusRequest,
            shared_types::ComputerAgentStatusResponse,
            shared_types::ComputerAgentStopRequest,
            shared_types::ComputerAgentStopResponse,
            shared_types::ComputerAgentCancelRequest,
            shared_types::ComputerAgentCancelResponse,
            // RCoder Agent 类型
            shared_types::RcoderChatRequest,
            shared_types::RcoderAgentCancelRequest,
            shared_types::RcoderAgentCancelResponse,
            shared_types::RcoderAgentStopRequest,
            shared_types::RcoderAgentStopResponse,
            shared_types::AgentStatusResponse,
            // 通用类型
            shared_types::HealthResponse,
        )),
        tags(
            (name = "Computer Agent", description = "Computer Agent HTTP API"),
            (name = "RCoder Agent", description = "RCoder Agent HTTP API"),
            (name = "System", description = "系统管理接口")
        )
    )]
    struct ApiDoc;

    SwaggerUi::new("/api/docs").url("/api-docs/openapi.json", ApiDoc::openapi())
}
