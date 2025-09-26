use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::{config::AppConfig, handler, proxy_agent::LocalSetAgentRequest};
use axum::Json;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

/// 会话信息结构
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub user_id: String,
    pub project_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_activity: chrono::DateTime<chrono::Utc>,
}

/// 应用状态
#[derive(Clone, Debug)]
pub struct AppState {
    /// 活跃的会话映射, project_id -> SessionInfo
    pub sessions: Arc<DashMap<String, SessionInfo>>,
    /// 应用配置
    pub config: AppConfig,

    /// 本地任务发送器
    pub local_task_sender: mpsc::UnboundedSender<LocalSetAgentRequest>,
}

/// 创建 Axum 路由
pub fn create_router(state: Arc<AppState>) -> Router {
    let api_routes = Router::new()
        .route("/health", get(handler::health_check))
        .route("/chat", post(handler::handle_chat))
        .route("/project/read", post(handler::handle_project_read))
        .route("/project/zip", post(handler::handle_project_zip))
        .route(
            "/project/download/{project_id}",
            get(handler::handle_project_download),
        )
        .route(
            "/agent/progress/{session_id}",
            get(handler::agent_session_notification),
        )
        .route("/agent/session/cancel", post(handler::agent_session_cancel))
        .with_state(state.clone());

    Router::new()
        .merge(api_routes)
        .merge(create_swagger_ui())
}

/// OpenAPI 文档结构
#[derive(OpenApi)]
#[openapi(
    paths(
        handler::health_check,
        handler::handle_chat,
        handler::handle_project_read,
        handler::handle_project_zip,
        handler::handle_project_download,
    ),
    components(
        schemas(
            handler::HealthResponse,
            handler::ChatRequest,
            handler::ChatResponse,
            handler::ProjectReadRequest,
            handler::ProjectZipRequest,
            nuwax_parser::ProjectSourceCode,
            nuwax_parser::FileInfo,
        )
    ),
    tags(
        (name = "system", description = "系统相关接口"),
        (name = "chat", description = "聊天相关接口"),
        (name = "project", description = "项目管理相关接口"),
    ),
    info(
        description = "RCoder AI 服务 API 文档",
        title = "RCoder AI API",
        version = "1.0.0",
        license(name = "MIT OR Apache-2.0"),
        contact(
            name = "RCoder Team",
            email = "team@rcoder.com"
        )
    ),
    servers(
        (url = "http://localhost:3000", description = "开发环境"),
        (url = "https://api.rcoder.com", description = "生产环境")
    )
)]
pub struct ApiDoc;

/// 创建 Swagger UI 路由
pub fn create_swagger_ui() -> SwaggerUi {
    SwaggerUi::new("/api/docs")
        .url("/api/docs/openapi.json", ApiDoc::openapi())
        .config(utoipa_swagger_ui::Config::new(["/api/docs/openapi.json"]))
}
