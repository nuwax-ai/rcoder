//! HTTP 路由定义
//!
//! 定义所有 HTTP 端点和路由

#![allow(dead_code)]

use axum::{
    Json, Router,
    routing::{get, post},
};
use dashmap::DashMap;
use std::sync::Arc;
use tower_http::limit::RequestBodyLimitLayer;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::api_key_manager::ApiKeyManager;
use crate::config::AppConfig;
use crate::service::AgentSessionService;
use crate::service::local_agent_service::LocalAgentHttpService;

/// HTTP 应用状态
#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub agent_session_service: Arc<AgentSessionService>,
    pub api_key_manager: Arc<ApiKeyManager>,
    pub shared_api_key_manager: Arc<DashMap<String, shared_types::ModelProviderConfig>>,
    pub project_uuid_map: Arc<DashMap<String, String>>,
    /// P0-1: agent management state
    pub agent_mgmt_http_state: Option<super::handlers::agent_mgmt_handler::AgentMgmtHttpState>,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        agent_session_service: Arc<AgentSessionService>,
        shared_api_key_manager: Arc<DashMap<String, shared_types::ModelProviderConfig>>,
    ) -> Self {
        Self {
            config,
            agent_session_service: agent_session_service.clone(),
            api_key_manager: Arc::new(ApiKeyManager::from_shared(shared_api_key_manager.clone())),
            shared_api_key_manager,
            project_uuid_map: Arc::new(DashMap::new()),
            agent_mgmt_http_state: None,
        }
    }

    /// P0-1: 设置 agent management 状态(在 main.rs 中注册)
    pub fn with_agent_mgmt(
        mut self,
        registry: std::sync::Arc<crate::agent_mgmt::AgentRegistry>,
        path_manager: crate::agent_mgmt::PathManager,
    ) -> Self {
        self.agent_mgmt_http_state = Some(
            super::handlers::agent_mgmt_handler::AgentMgmtHttpState::new(registry, path_manager),
        );
        self
    }

    /// 创建 LocalAgentHttpService 实例用于 RCoder 模式
    pub fn create_local_agent_service(&self) -> Arc<LocalAgentHttpService> {
        Arc::new(LocalAgentHttpService::new(
            self.agent_session_service.clone(),
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
        pod_count, rcoder_progress,
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
        .route("/computer/pod/count", get(pod_count::handle_pod_count))
        .with_state(state.clone());

    // RCoder Agent 路由（使用 LocalAgentHttpService）
    let rcoder_routes = Router::new()
        .route(
            "/chat",
            post(http_handlers::handle_chat::<LocalAgentHttpService>),
        )
        .route(
            "/agent/session/cancel",
            post(http_handlers::handle_cancel::<LocalAgentHttpService>),
        )
        .route(
            "/agent/stop",
            post(http_handlers::handle_stop::<LocalAgentHttpService>),
        )
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

    // P0-1: Agent 管理路由(仅在 agent_mgmt_http_state 被设置时启用)
    let agent_mgmt_routes = if let Some(am_state) = state.agent_mgmt_http_state.clone() {
        create_agent_mgmt_router(am_state)
    } else {
        Router::new()
    };

    // 组合路由
    Router::new()
        .merge(computer_routes)
        .merge(rcoder_routes)
        .merge(api_routes)
        .merge(agent_mgmt_routes)
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

/// P0-1: 构建 agent_mgmt 子路由(导出供集成测试使用)
pub fn create_agent_mgmt_router(
    state: super::handlers::agent_mgmt_handler::AgentMgmtHttpState,
) -> Router {
    use super::handlers::agent_mgmt_handler::{
        check_agent, get_agent, install_agent, install_from_npm, install_from_url, list_agents,
        uninstall_agent,
    };
    Router::new()
        .route("/agent-mgmt/agents/list", post(list_agents))
        .route("/agent-mgmt/agents/get", post(get_agent))
        .route("/agent-mgmt/agents/check", post(check_agent))
        .route("/agent-mgmt/agents/install", post(install_agent))
        .route("/agent-mgmt/agents/install-from-url", post(install_from_url))
        .route("/agent-mgmt/agents/install-from-npm", post(install_from_npm))
        .route("/agent-mgmt/agents/uninstall", post(uninstall_agent))
        .with_state(state)
}

/// 创建 Swagger UI
fn create_swagger_ui() -> SwaggerUi {
    use super::handlers::{
        computer_cancel::__path_handle_computer_cancel, computer_chat::__path_handle_computer_chat,
        computer_progress::__path_handle_computer_progress,
        computer_status::__path_handle_computer_status, computer_stop::__path_handle_computer_stop,
        pod_count::__path_handle_pod_count,
        agent_mgmt_handler::{
            __path_list_agents, __path_get_agent, __path_check_agent,
            __path_install_from_url, __path_install_from_npm, __path_uninstall_agent,
        },
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
            // Pod 管理端点
            handle_pod_count,
            // 健康检查
            health_check,
            // Agent Management 端点
            list_agents,
            get_agent,
            check_agent,
            install_from_url,
            install_from_npm,
            uninstall_agent,
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
            // Pod 管理类型
            shared_types::PodCountResponse,
            shared_types::PodCountByServiceType,
            // Agent Management 类型
            shared_types::RoutingParams,
            shared_types::ListAgentsRequest,
            shared_types::ListAgentsResponse,
            shared_types::GetAgentRequest,
            shared_types::CheckAgentRequest,
            shared_types::CheckAgentResponse,
            shared_types::AgentIdentity,
            shared_types::InstallFromUrlRequest,
            shared_types::InstallFromPackageManagerRequest,
            shared_types::InstallAgentResponse,
            shared_types::UninstallAgentRequest,
            shared_types::UninstallAgentResponse,
            shared_types::PlatformEntry,
            shared_types::AgentInfo,
            shared_types::AgentDetailInfo,
            shared_types::InstallType,
            shared_types::AgentInstallStatus,
            shared_types::InstallAction,
            shared_types::SystemInfo,
            shared_types::StaticCheckResult,
        )),
        tags(
            (name = "Computer Agent", description = "Computer Agent HTTP API"),
            (name = "RCoder Agent", description = "RCoder Agent HTTP API"),
            (name = "pod", description = "Pod 容器管理接口"),
            (name = "system", description = "系统管理接口"),
            (name = "agent-mgmt", description = "Agent 二进制安装/卸载/检查接口，支持多平台 URL 安装和版本管理"),
        )
    )]
    struct ApiDoc;

    SwaggerUi::new("/api/docs").url("/api-docs/openapi.json", ApiDoc::openapi())
}
