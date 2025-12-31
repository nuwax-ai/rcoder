use std::sync::Arc;

use axum::{Router, routing::get};
use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::mpsc;

use crate::{config::AppConfig, handler, proxy_agent::LocalSetAgentRequest};
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
#[derive(Clone)]
pub struct AppState {
    /// 活跃的会话映射, project_id -> SessionInfo
    pub sessions: Arc<DashMap<String, SessionInfo>>,
    /// 应用配置
    pub config: AppConfig,

    /// 本地任务发送器
    pub local_task_sender: mpsc::UnboundedSender<LocalSetAgentRequest>,

    /// Pingora 代理服务引用（用于读取真实指标）
    pub pingora_service: Option<Arc<rcoder_proxy::PingoraProxyService>>,

    /// 🔒 API 密钥管理器（用于 Pingora 代理注入真实密钥）
    /// 注意：此现在是 shared_api_key_manager 的包装器
    pub api_key_manager: Arc<crate::api_key_manager::ApiKeyManager>,

    /// 🔒 共享的 API 密钥 DashMap（直接与 Pingora 共享）
    /// 存储格式：<UUID> -> ModelProviderConfig
    pub shared_api_key_manager: Arc<DashMap<String, shared_types::ModelProviderConfig>>,

    /// 🔒 project_id -> service_uuid 映射（用于清理时查找对应的配置）
    pub project_uuid_map: Arc<DashMap<String, String>>,
}

/// 创建 Axum 路由
pub fn create_router(state: Arc<AppState>) -> Router {
    let api_routes = Router::new()
        .route("/health", get(handler::health_check))
        .with_state(state.clone());

    Router::new().merge(api_routes).merge(create_swagger_ui())
}

/// OpenAPI 文档结构
#[derive(OpenApi)]
#[openapi(
    paths(
        handler::health_check,
    ),
    components(
        schemas(
            // 响应结构体
            handler::HealthResponse,
        )
    ),
    tags(
        (name = "system", description = "系统健康检查和状态监控接口"),
    ),
    info(
        description = r#"
RCoder AI 服务 API

基于 ACP (Agent Client Protocol) 的 AI 驱动开发平台，提供完整的 AI 代理集成解决方案。

## 主要功能

- **智能对话**: 支持文本、图像、音频、文档等多媒体内容的 AI 交互
- **实时通知**: 通过 SSE 协议提供 AI 代理执行进度的实时推送
- **会话管理**: 完整的会话生命周期管理，支持任务取消
- **项目隔离**: 每个对话在独立的项目工作空间中进行，确保安全性

## 技术架构

- **协议**: ACP (Agent Client Protocol) v0.4
- **gRPC 通信**: rcoder 通过 gRPC 与 agent_runner 通信
- **健康检查**: 提供服务健康状态查询

## gRPC 接口

agent_runner 主要通过 gRPC 提供服务：
- `Chat` - 聊天对话
- `SubscribeProgress` - 订阅进度事件（Server Streaming）
- `CancelSession` - 取消会话
- `GetStatus` - 查询状态
"#,
        title = "RCoder AI API",
        version = "2.0.0",
        license(name = "MIT OR Apache-2.0", url = "https://opensource.org/licenses/MIT"),
        contact(
            name = "RCoder Team",
            email = "team@rcoder.com",
            url = "https://github.com/rcoder/rcoder"
        )
    ),
    servers(
        (url = "http://localhost:50051", description = "gRPC 服务 (agent_runner)"),
        (url = "http://localhost:8087", description = "HTTP API 服务 (rcoder)"),
    )
)]
pub struct ApiDoc;

/// 创建 Swagger UI 路由
pub fn create_swagger_ui() -> SwaggerUi {
    SwaggerUi::new("/api/docs")
        .url("/api/docs/openapi.json", ApiDoc::openapi())
        .config(utoipa_swagger_ui::Config::new(["/api/docs/openapi.json"]))
}
