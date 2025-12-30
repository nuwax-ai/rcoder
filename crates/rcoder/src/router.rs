use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};
use serde::Serialize;
use shared_types::ProjectAndContainerInfo;

use crate::{config::AppConfig, handler, storage::ProjectAdapter};
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
    /// 应用配置
    pub config: AppConfig,
    /// 项目适配器 - 统一管理项目、会话和容器数据（替代原有的 3 个 DashMap）
    pub projects: ProjectAdapter,
    /// Pingora 代理服务引用（用于读取真实指标）
    pub pingora_service: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    /// gRPC 连接池（用于与 agent_runner 通信）
    pub grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        pingora: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    ) -> Self {
        Self {
            config,
            projects: ProjectAdapter::new().expect("初始化 ProjectAdapter 失败"),
            pingora_service: pingora,
            grpc_pool: Arc::new(crate::grpc::GrpcChannelPool::new()),
        }
    }

    // ========== 向后兼容的便捷方法 ==========

    /// 获取项目信息（替代 project_and_agent_map.get）
    #[inline]
    pub fn get_project(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        self.projects.get(project_id)
    }

    /// 插入项目信息（替代 project_and_agent_map.insert）
    #[inline]
    pub fn insert_project(&self, project_id: String, info: Arc<ProjectAndContainerInfo>) {
        if let Err(e) = self.projects.insert(project_id.clone(), info) {
            tracing::error!("插入项目 {} 失败: {}", project_id, e);
        }
    }

    /// 删除项目（替代 project_and_agent_map.remove）
    #[inline]
    pub fn remove_project(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        self.projects.remove(project_id)
    }

    /// 检查项目是否存在（替代 project_and_agent_map.contains_key）
    #[inline]
    pub fn contains_project(&self, project_id: &str) -> bool {
        self.projects.contains_key(project_id)
    }

    /// 通过会话ID获取项目信息（替代 sessions.get）
    #[inline]
    pub fn get_by_session(&self, session_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        self.projects.get_by_session_id(session_id)
    }

    /// 通过会话ID获取容器ID（替代 session_to_container_id.get）
    #[inline]
    pub fn get_container_id_by_session(&self, session_id: &str) -> Option<String> {
        self.projects.get_container_id_by_session(session_id)
    }

    /// 更新会话信息
    #[inline]
    pub fn update_session(&self, project_id: &str, session_id: &str) {
        if let Err(e) = self.projects.update_session(project_id, session_id) {
            tracing::error!(
                "更新会话失败: project_id={}, session_id={}, error={}",
                project_id,
                session_id,
                e
            );
        }
    }

    /// 更新项目活动时间，返回实际更新使用的时间戳
    #[inline]
    pub fn update_activity(&self, project_id: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        self.projects.update_activity(project_id)
    }

    /// 更新会话活动时间
    #[inline]
    pub fn update_session_activity(&self, session_id: &str) {
        self.projects.update_session_activity(session_id);
    }
}

/// 创建 Axum 路由
pub fn create_router(state: Arc<AppState>) -> Router {
    let api_routes = Router::new()
        .route("/chat", post(handler::handle_chat))
        // Axum SSE 代理处理器，直接返回 SSE 流
        .route(
            "/agent/progress/{session_id}",
            get(handler::agent_session_notification),
        )
        .route("/agent/session/cancel", post(handler::agent_session_cancel))
        .route("/agent/stop", post(handler::agent_stop))
        .route("/agent/status/{project_id}", get(handler::agent_status))
        .with_state(state.clone());

    // Computer Agent Runner 路由
    let computer_routes = Router::new()
        .route("/computer/chat", post(handler::handle_computer_chat))
        .route("/computer/agent/stop", post(handler::computer_agent_stop))
        .route(
            "/computer/agent/session/cancel",
            post(handler::computer_agent_session_cancel),
        )
        // 进度流复用现有的 agent_session_notification
        .route(
            "/computer/progress/{session_id}",
            get(handler::agent_session_notification),
        )
        // computer agent 专用进度流接口（使用与 agent_session_notification 相同的逻辑）
        .route(
            "/computer/agent/progress/{session_id}",
            get(handler::computer_agent_progress_notification),
        )
        // VNC 桌面访问说明接口
        .route(
            "/computer/desktop/{user_id}/{project_id}",
            get(handler::computer_desktop_vnc),
        )
        // Pod 容器管理接口
        .route("/computer/pod/count", get(handler::pod_count))
        .route("/computer/pod/list", get(handler::pod_list))
        .route("/computer/pod/ensure", post(handler::pod_ensure))
        .route("/computer/pod/keepalive", post(handler::pod_keepalive))
        .route("/computer/pod/restart", post(handler::pod_restart))
        .route("/computer/pod/status", get(handler::pod_status))
        .route("/computer/pod/vnc-status", get(handler::pod_vnc_status))
        .with_state(state.clone());

    // Pingora 代理 API 路由（用于文档和状态查询）
    let proxy_api_routes = Router::new()
        .route("/proxy/status", get(handler::proxy_status))
        .route("/proxy/stats", get(handler::proxy_stats))
        .route("/proxy/config", get(handler::proxy_config))
        .with_state(state.clone());

    // 调试路由（仅用于开发和问题排查）
    let debug_routes = Router::new()
        .route("/debug/sql", post(handler::debug_sql_query))
        .route("/debug/projects", get(handler::debug_list_projects))
        .route("/debug/containers", get(handler::debug_list_containers))
        .route("/debug/storage/stats", get(handler::debug_storage_stats))
        .with_state(state.clone());

    // 健康检查路由
    let health_routes = Router::new()
        .route("/health", get(handler::health_check))
        .with_state(state.clone());

    Router::new()
        .merge(health_routes)
        .merge(api_routes)
        .merge(computer_routes)
        .merge(proxy_api_routes)
        .merge(debug_routes) // 添加调试路由
        .merge(create_swagger_ui())
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024)) // 50MB body 大小限制
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
        handler::handle_computer_chat,
        handler::computer_agent_stop,
        handler::computer_agent_session_cancel,
        handler::computer_agent_progress_notification,
        handler::computer_desktop_vnc,
        handler::computer_desktop_proxy,
        handler::pod_count,
        handler::pod_list,
        handler::pod_ensure,
        handler::pod_keepalive,
        handler::pod_restart,
        handler::pod_status,
        handler::pod_vnc_status,
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
            shared_types::ChatResponse,
            handler::StopAgentResponse,
            handler::CancelResponse,
            // 移除 SessionUpdateEvent，因为现在使用 ProxyRedirectResponse
            handler::ProxyErrorResponse,
            // 模型配置相关结构体
            shared_types::ModelProviderConfig,
            shared_types::ModelApiProtocol,
            shared_types::ModelProviderSafeInfo,
            // Agent状态相关结构体
            shared_types::AgentStatusResponse,
            shared_types::AgentStatus,
            handler::SessionNotificationParams,
            // SSE 进度事件结构体（用于文档）
            handler::ProgressEventDoc,
            handler::SseErrorEvent,
            // 附件相关结构体
            shared_types::Attachment,
            shared_types::AttachmentSource,
            shared_types::TextAttachment,
            shared_types::ImageAttachment,
            shared_types::AudioAttachment,
            shared_types::DocumentAttachment,
            shared_types::ImageDimensions,
            // 会话消息相关结构体
            shared_types::UnifiedSessionMessage,
            shared_types::SessionMessageType,
            // Computer Agent 相关结构体
            handler::ComputerChatRequest,
            handler::ComputerAgentStopRequest,
            handler::ComputerAgentStopResponse,
            handler::DesktopPathParams,
            handler::VncProxyPathParams,
            handler::DesktopAccessResponse,
            handler::DesktopErrorResponse,
            // Pod 容器管理相关结构体
            handler::PodCountResponse,
            handler::PodCountByServiceType,
            handler::PodListQuery,
            handler::PodListResponse,
            handler::PodDetailInfo,
            handler::EnsurePodRequest,
            handler::PodResourceLimits,
            handler::EnsurePodResponse,
            handler::PodContainerInfo,
            handler::KeepalivePodRequest,
            handler::KeepalivePodResponse,
            handler::RestartPodRequest,
            handler::RestartPodResponse,
            handler::PodStatusQuery,
            handler::PodStatusResponse,
            handler::VncStatusQuery,
            handler::VncStatusResponse,
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
        (name = "computer", description = "Computer Agent 桌面与聊天接口"),
        (name = "pod", description = "Pod 容器管理接口，支持容器监控、启动和保活"),
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

- **VNC 代理**: `/computer/vnc/{user_id}/{project_id}/{*path}` - 代理到容器的 noVNC 服务（端口 6080）
  - 路径示例：`/computer/vnc/user_123/proj_456/vnc.html` - VNC 桌面页面
  - WebSocket：`/computer/vnc/user_123/proj_456/websockify` - VNC 连接
- **端口路由**: `/proxy/{port}/{*path}` - 动态路由到任意端口的后端服务
  - 支持两种方式：直接访问 Pingora 端口 或 通过 API 重定向
- **负载均衡**: 支持 Round Robin 算法和健康检查
- **动态发现**: 自动发现和添加后端服务，无需预配置
- **高性能**: 基于 Rust 异步 I/O 的高性能代理

## 使用流程

1. 调用 `/chat` 接口发送对话请求
2. 通过 `/agent/progress/{session_id}` 建立 SSE 连接接收实时更新
3. 可随时通过 `/agent/session/cancel` 取消正在执行的任务
4. 直接访问 Pingora 代理路径或使用管理接口

## 代理接口示例

- `GET /proxy/status` - 查看代理服务状态
- `GET /proxy/stats` - 查看代理统计信息
- `GET /proxy/config` - 查看代理配置信息
- 直接访问 `http://{host}:{pingora_port}/proxy/{port}/{path}` - 使用 Pingora 代理服务
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
        (url = "http://localhost:8087", description = "本地开发环境"),
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
