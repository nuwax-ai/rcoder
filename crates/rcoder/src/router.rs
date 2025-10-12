use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};
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
}

/// 创建 Axum 路由
pub fn create_router(state: Arc<AppState>) -> Router {
    let api_routes = Router::new()
        .route("/health", get(handler::health_check))
        .route("/chat", post(handler::handle_chat))
        .route(
            "/agent/progress/{session_id}",
            get(handler::agent_session_notification),
        )
        .route("/agent/session/cancel", post(handler::agent_session_cancel))
        .route("/agent/stop", post(handler::agent_stop))
        .route("/agent/status/{project_id}", get(handler::agent_status))
        .with_state(state.clone());

    // Pingora 代理 API 路由（用于文档和状态查询）
    let proxy_api_routes = Router::new()
        .route("/proxy/status", get(handler::proxy_status))
        .route("/proxy/stats", get(handler::proxy_stats))
        .route("/proxy/config", get(handler::proxy_config))
        .route("/proxy", get(handler::proxy_with_query_params))
        .route("/proxy/{port}", get(handler::proxy_to_port))
        .route(
            "/proxy/{port}/{*path}",
            get(handler::proxy_to_port_with_path),
        )
        .with_state(state.clone());

    Router::new()
        .merge(api_routes)
        .merge(proxy_api_routes)
        .merge(create_swagger_ui())
}

/// OpenAPI 文档结构
#[derive(OpenApi)]
#[openapi(
    paths(
        handler::health_check,
        handler::handle_chat,
        handler::agent_session_notification,
        handler::agent_session_cancel,
        handler::agent_stop,
        handler::agent_status,
        // Pingora 代理接口
        handler::proxy_status,
        handler::proxy_stats,
        handler::proxy_config,
        handler::proxy_to_port,
        handler::proxy_to_port_with_path,
        handler::proxy_with_query_params,
    ),
    components(
        schemas(
            // 响应结构体
            handler::HealthResponse,
            handler::ChatRequest,
            handler::ChatResponse,
            handler::StopAgentResponse,
            crate::handler::SessionUpdateEvent,
            // 模型配置相关结构体
            shared_types::ModelProviderConfig,
            shared_types::ModelApiProtocol,
            shared_types::ModelProviderSafeInfo,
            // Agent状态相关结构体
            crate::model::AgentStatusResponse,
            crate::model::AgentStatus,
            // 附件相关结构体
            crate::model::Attachment,
            crate::model::AttachmentSource,
            crate::model::TextAttachment,
            crate::model::ImageAttachment,
            crate::model::AudioAttachment,
            crate::model::DocumentAttachment,
            crate::model::ImageDimensions,
            // 会话消息相关结构体
            crate::model::UnifiedSessionMessage,
            crate::model::SessionMessageType,
            // Pingora 代理相关结构体
            handler::ProxyResponse,
            handler::ProxyStatus,
            handler::ProxyStats,
            handler::ProxyConfig,
            handler::ProxyPathParams,
            handler::ProxyPathWithTailParams,
            handler::ProxyErrorResponse,
            handler::LoadBalancerInfo,
            handler::BackendInfo,
            handler::PortStats,
            handler::HealthCheckConfig,
        )
    ),
    tags(
        (name = "system", description = "系统健康检查和状态监控接口"),
        (name = "chat", description = "AI 聊天对话接口，支持多媒体内容"),
        (name = "agent", description = "AI 代理会话管理和实时通知接口"),
        (name = "proxy", description = "Pingora 反向代理接口，支持端口路由和负载均衡"),
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
- **Pingora 反向代理**: 基于 Cloudflare Pingora 的高性能反向代理服务

## 技术架构

- **协议**: ACP (Agent Client Protocol) v0.4
- **代理类型**: 支持 Codex、Claude、Proxy 三种 AI 代理
- **并发**: 基于 MPMC 架构的高并发处理
- **实时通信**: Server-Sent Events (SSE) 协议
- **反向代理**: Cloudflare Pingora 高性能代理服务器

## Pingora 代理功能

- **端口路由**: `/proxy/{port}/{path}` - 动态路由到任意端口的后端服务
- **负载均衡**: 支持 Round Robin 算法和健康检查
- **动态发现**: 自动发现和添加后端服务，无需预配置
- **高性能**: 基于 Rust 异步 I/O 的高性能代理

## 使用流程

1. 调用 `/chat` 接口发送对话请求
2. 通过 `/agent/progress/{session_id}` 建立 SSE 连接接收实时更新
3. 可随时通过 `/agent/session/cancel` 取消正在执行的任务
4. 使用 `/proxy/{port}/{path}` 代理请求到任意后端服务

## 代理接口示例

- `GET /proxy/status` - 查看代理服务状态
- `GET /proxy/3000/api/users` - 代理到端口 3000 的服务
- `GET /proxy/8080/health` - 代理到端口 8080 的健康检查
- `GET /proxy/stats` - 查看代理统计信息
"#,
        title = "RCoder AI API",
        version = "1.0.0",
        license(name = "MIT OR Apache-2.0", url = "https://opensource.org/licenses/MIT"),
        contact(
            name = "RCoder Team",
            email = "team@rcoder.com",
            url = "https://github.com/rcoder/rcoder"
        )
    ),
    servers(
        (url = "http://localhost:3000", description = "本地开发环境"),
        (url = "https://api.rcoder.com", description = "生产环境"),
        (url = "https://staging-api.rcoder.com", description = "测试环境")
    )
)]
pub struct ApiDoc;

/// 创建 Swagger UI 路由
pub fn create_swagger_ui() -> SwaggerUi {
    SwaggerUi::new("/api/docs")
        .url("/api/docs/openapi.json", ApiDoc::openapi())
        .config(utoipa_swagger_ui::Config::new(["/api/docs/openapi.json"]))
}
