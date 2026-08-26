//! OpenAPI 文档聚合与文档 UI（从 router.rs 拆出；路由组装仍在 router.rs）。
//!
//! [`ApiDoc`] 是 rcoder 主文档的 utoipa 声明（paths/components 全量），
//! [`create_swagger_ui`] 聚合两份文档（主文档 + file-server 全量文档，
//! UI 顶部下拉切换），[`create_scalar_docs`] 以 Scalar 界面提供同样两份
//! 文档（每份独立页面，供对比选用）。

use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::handler;

/// OpenAPI 文档结构
#[derive(OpenApi)]
#[openapi(
    paths(
        handler::health_check,
        handler::handle_chat,
        handler::agent_session_notification,
        handler::agent_session_cancel,
        handler::agent_notify_resolved,
        handler::agent_stop,
        handler::agent_status,
        handler::handle_computer_chat,
        handler::computer_agent_stop,
        handler::computer_agent_status, // 🆕 新增
        handler::computer_agent_session_cancel,
        handler::computer_notify_resolved,
        handler::computer_agent_progress_notification,
        handler::computer_desktop_vnc,
        handler::computer_desktop_proxy,
        handler::computer_audio_proxy,
        handler::computer_ime_proxy,
        handler::computer_db_reset_password,
        handler::computer_db_create_database,
        handler::computer_ttyd_proxy,
        handler::pod_count,
        handler::pod_list,
        handler::pod_ensure,
        handler::pod_keepalive,
        handler::pod_restart,
        handler::pod_status,
        handler::computer_cache_clean,
        handler::pod_vnc_status,
        // Pingora 代理接口
        handler::proxy_status,
        handler::proxy_stats,
        handler::proxy_config,
        handler::proxy_to_port,
        handler::proxy_to_port_with_path,
        handler::proxy_to_app_with_path,
        handler::proxy_to_devapp_with_path,
        handler::proxy_to_userapp_ttyd,
        handler::proxy_to_userapp_vnc,
        handler::proxy_to_userapp_audio,
        handler::proxy_to_userapp_ime,
        handler::proxy_to_userapp_runtime_ttyd,
        handler::proxy_to_userapp_runtime_pgweb,
        handler::proxy_to_dev_dbx,
        handler::proxy_to_prod_dbx,
        handler::userapp_proxy_routes_doc,
        handler::proxy_with_query_params,
        // P0-4: Agent Management 转发层
        handler::list_agents,
        handler::get_agent,
        handler::check_agent,
        handler::install_agent,
        handler::install_from_url,
        handler::install_from_npm,
        handler::uninstall_agent,
        // DevComputer 调试接口
        handler::handle_devcomputer_chat,
        handler::devcomputer_agent_stop,
        handler::devcomputer_agent_status,
        handler::devcomputer_agent_session_cancel,
        handler::devcomputer_notify_resolved,
        handler::devcomputer_agent_progress_notification,
        // 应用管理接口
        app_manager::handlers::query_apps,
        app_manager::handlers::get_app,
        app_manager::handlers::update_app,
        app_manager::handlers::delete_app,
        app_manager::handlers::start_app,
        app_manager::handlers::stop_app,
        app_manager::handlers::restart_app,
        app_manager::handlers::set_recycle_policy,
        app_manager::handlers::query_app_log_sources,
        app_manager::handlers::query_app_logs,
        app_manager::handlers::get_app_health,
        app_manager::handlers::get_app_stats,
        app_manager::handlers::get_app_events,
        app_manager::handlers::upload_file,
        app_manager::handlers::list_files,
        app_manager::handlers::delete_file,
        app_manager::handlers::list_app_runtimes,
        app_manager::handlers::get_app_storage,
        app_manager::handlers::clear_app_storage,
        app_manager::handlers::destroy_app_storage,
        app_manager::handlers::query_storage,
        app_manager::handlers::reset_db_password,
        app_manager::handlers::create_database,
        app_manager::handlers::stream_app_logs_v1,
        app_manager::handlers::upload_from_url,
        crate::userapp_forward::db::align_credentials,
    ),
    components(
        schemas(
            // userApp 转发层（PG 凭据对齐；create-workspace 为内部接口不入文档）
            shared_types::AlignCredentialsRequest,
            shared_types::AlignCredentialsOutcome,
            // 响应结构体
            shared_types::HealthCheckResponse,
            shared_types::AgentChatRequest,
            shared_types::ChatResponse,
            shared_types::AgentStopResponse,
            shared_types::AgentCancelResponse,
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
            // Permission 相关结构体
            shared_types::ResolvePermissionResponseDto,
            // Computer Agent 相关结构体
            shared_types::ComputerChatRequest,
            shared_types::ComputerAgentStopRequest,
            shared_types::ComputerAgentStopResponse,
            shared_types::ComputerAgentStatusRequest,
            shared_types::ComputerAgentStatusResponse,
            handler::DesktopPathParams,
            handler::VncProxyPathParams,
            handler::AudioProxyPathParams,
            handler::ImeProxyPathParams,
            handler::TtydProxyPathParams,
            handler::DesktopAccessResponse,
            handler::DesktopErrorResponse,
            // Pod 容器管理相关结构体
            shared_types::PodCountResponse,
            shared_types::PodCountByServiceType,
            handler::PodListQuery,
            handler::PodListResponse,
            handler::PodDetailInfo,
            handler::EnsurePodRequest,
            shared_types::ServiceResourceLimits,
            handler::EnsurePodResponse,
            handler::PodContainerInfo,
            handler::KeepalivePodRequest,
            handler::KeepalivePodResponse,
            handler::RestartPodRequest,
            handler::RestartPodResponse,
            handler::CacheCleanRequest,
            handler::CacheCleanResponse,
            handler::PodStatusQuery,
            handler::PodStatusResponse,
            handler::VncStatusQuery,
            shared_types::VncStatusResponse,
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
            // P0-4: Agent Management 类型
            shared_types::AgentInfo,
            shared_types::AgentDetailInfo,
            shared_types::AgentInstallStatus,
            shared_types::InstallType,
            shared_types::InstallAction,
            shared_types::PlatformEntry,
            shared_types::ListAgentsRequest,
            shared_types::ListAgentsResponse,
            shared_types::CheckAgentRequest,
            shared_types::CheckAgentResponse,
            shared_types::AgentIdentity,
            shared_types::InstallFromUrlRequest,
            shared_types::InstallFromPackageManagerRequest,
            shared_types::InstallAgentResponse,
            shared_types::UninstallAgentRequest,
            shared_types::UninstallAgentResponse,
            shared_types::StaticCheckResult,
            shared_types::SystemInfo,
            shared_types::RoutingParams,
            shared_types::GetAgentRequest,
            // multipart 特有类型(rcoder 本地)
            handler::InstallMetadataBody,
            handler::InstallMultipartBody,
            // 应用管理相关结构体
            app_manager::models::StartAppRequest,
            app_manager::models::StartAppResult,
            app_manager::models::StartPgCredential,
            app_manager::models::AppInfo,
            app_manager::models::AppRuntimeInfo,
            app_manager::models::AppStatus,
            app_manager::models::QueryAppsRequest,
            app_manager::models::UpdateAppRequest,
            app_manager::models::ResourceStats,
            app_manager::models::HealthInfo,
            app_manager::models::PaginatedResponse<app_manager::models::AppRuntimeInfo>,
            app_manager::models::PaginatedResponse<app_manager::models::StorageInfo>,
            app_manager::models::Condition,
            app_manager::models::DeleteAppRequest,
            app_manager::models::DestroyStorageRequest,
            app_manager::models::QueryStorageRequest,
            app_manager::models::StorageFilters,
            app_manager::models::StorageInfo,
            app_manager::models::ResetDbPasswordRequest,
            app_manager::models::CreateDatabaseRequest,
            container_runtime_api::AppPortStatus,
            container_runtime_api::AppEventInfo,
        )
    ),
    tags(
        (name = "system", description = "系统健康检查和状态监控接口"),
        (name = "chat", description = "AI 聊天对话接口，支持多媒体内容"),
        (name = "agent", description = "AI 代理会话管理和实时通知接口"),
        (name = "computer", description = "Computer Agent 桌面与聊天接口"),
        (name = "pod", description = "Pod 容器管理接口，支持容器监控、启动和保活"),
        (name = "proxy", description = "Pingora 反向代理接口，支持端口路由和负载均衡"),
        (name = "agent-mgmt", description = "Agent 二进制安装/卸载/检查接口(P0-4: rcoder 转发到 agent_runner 容器)"),
        (name = "devcomputer", description = "DevComputer 调试接口（与 /computer 共享容器，自动注入 auto_reload 配置）"),
        (name = "应用管理", description = "应用容器管理接口，支持创建、启动、停止、删除应用"),
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
        license(name = "Apache-2.0", url = "https://www.apache.org/licenses/LICENSE-2.0"),
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

/// 创建 Swagger UI 路由。
///
/// 内部路由 path（file-server-proxy 分流代理的上游镜像接口）：路由保留、对外
/// 文档不暴露——Java 同事调 computer 域同名接口，带 `x-service-type: userapp`
/// header 经 60000 分流代理内部路由到这些；直接暴露会让调用方绕过分流契约。
const INTERNAL_USERAPP_PATHS: [&str; 13] = [
    "/api/userapp/download-all-files",
    "/api/userapp/files-update",
    "/api/userapp/generate-file",
    "/api/userapp/get-file-list",
    "/api/userapp/import-project",
    "/api/userapp/execute-command",
    "/api/userapp/push-skills-to-workspace",
    "/api/userapp/resolve-file",
    "/api/userapp/search-files",
    "/api/userapp/upload-file",
    "/api/userapp/upload-files",
    "/api/userapp/workspace",
    "/api/userapp/zip-workspace",
];

/// 从文档剔除内部路由 path（components 中失去引用的 schema 残留无害，不追引
/// 用图清理）。
fn strip_internal_userapp_paths(document: &mut utoipa::openapi::OpenApi) {
    document
        .paths
        .paths
        .retain(|path, _| !INTERNAL_USERAPP_PATHS.contains(&path.as_str()));
}

/// 聚合两份文档（UI 顶部下拉切换）：rcoder 主文档 + file-server 文档。
/// file-server 全量文档（含 /api/userapp）始终聚合在此；实际路由宿主：
/// 老路径（project/computer/git/build）常驻 rcoder 主服务（`merged_router`），
/// userapp 域由 rcoder 转发层接管、本地实现在 per-app 开发容器内 file-server（60000）。
///
/// 主文档额外合入 userApp 业务域（file-server 的 `/api/userapp/*` 路径 +
/// schemas）——Swagger 默认打开主文档即见 userApp 全貌（dev 生命周期/编译/
/// 文件/静态），无需切下拉；其余 file-server 域（project/computer/git 等）
/// 仍只在 file-server.json，防主文档膨胀。两份文档均剔除
/// [`INTERNAL_USERAPP_PATHS`]（分流代理的内部路由面）。
pub fn create_swagger_ui() -> SwaggerUi {
    SwaggerUi::new("/api/docs")
        .url("/api/docs/openapi.json", primary_document())
        .url("/api/docs/file-server.json", file_server_document())
        .config(utoipa_swagger_ui::Config::new([
            "/api/docs/openapi.json",
            "/api/docs/file-server.json",
        ]))
}

/// Scalar 风格文档 UI（与 Swagger UI 并存试用，文档构造完全复用）。
///
/// 一实例一文档（Scalar 无多文档下拉），两份文档挂两个页面：
/// - `/api/docs/scalar`：主文档（应用管理 + userApp 业务域）
/// - `/api/docs/scalar/file-server`：file-server 全量文档
///
/// 注意：Scalar 的 UI JS 由页面从公网 CDN（jsdelivr）加载，spec 本身
/// 内嵌在返回的 HTML 中——浏览器无法出网时页面会白屏（Swagger UI 资产
/// 是编译期内嵌的，不受影响）；届时用 `custom_html` 换自托管 JS。
/// 路由共存依赖 matchit static 优先于 `/api/docs/{*rest}` 通配。
pub fn create_scalar_docs() -> axum::Router {
    use utoipa_scalar::{Scalar, Servable};
    // `.title()` 在已发布的 0.3.0 尚未提供（master 未发版），页面标题
    // 用默认 "Scalar"；升级 0.4+ 可补。
    axum::Router::new()
        .merge(Scalar::with_url("/api/docs/scalar", primary_document()))
        .merge(Scalar::with_url(
            "/api/docs/scalar/file-server",
            file_server_document(),
        ))
}

/// file-server 下拉文档 = file-server 全量文档（TS 对齐域）+ userApp 域文档
/// （file-server-userapp crate 独立产出）剔除内部路由 path。
fn file_server_document() -> utoipa::openapi::OpenApi {
    let mut doc = file_server::openapi::document(file_server::routes::api_router().into_openapi());
    doc.merge(file_server_userapp::document());
    strip_internal_userapp_paths(&mut doc);
    doc
}

/// 主文档 = rcoder 应用管理 + userApp 业务域（选择性合入）。
fn primary_document() -> utoipa::openapi::OpenApi {
    let mut doc = ApiDoc::openapi();
    let mut userapp = file_server_userapp::document();
    strip_internal_userapp_paths(&mut userapp);
    doc.merge(userapp);
    doc
}

#[cfg(test)]
mod openapi_tests {
    use super::*;
    use axum::Router;

    /// 运行日志 SSE 契约锚点：事件清单必须出现在 description 里（同事按 swagger
    /// 直读对接，描述被精简回一句话在此报红——对齐 file-server-userapp 同款测试）。
    #[test]
    fn app_logs_stream_description_carries_sse_contract() {
        let document = ApiDoc::openapi();
        let item = document
            .paths
            .paths
            .get("/api/v1/apps/{app_id}/logs/stream")
            .expect("logs/stream path documented");
        let op = item.post.as_ref().expect("POST operation");
        let resp = op
            .responses
            .responses
            .get("200")
            .expect("200 response present");
        let desc = match resp {
            utoipa::openapi::RefOr::Ref(_) => panic!("200 response is a $ref"),
            utoipa::openapi::RefOr::T(r) => r.description.clone(),
        };
        for token in [
            "log",
            "source_error",
            "source_recovered",
            "cursor_reset",
            "checkpoint",
            "heartbeat",
            "cursor",
        ] {
            assert!(
                desc.contains(token),
                "logs/stream 描述缺 SSE 事件锚点 {token}"
            );
        }
    }

    #[test]
    fn userapp_release_log_and_publish_paths_are_documented() {
        let document = ApiDoc::openapi();
        let paths = document.paths.paths;
        for path in [
            // releases 五接口已随 RBD 卷形态删除（部署只走 start+url，见 handbook 10）
            "/api/v1/apps/{app_id}/logs/query",
            "/api/v1/apps/{app_id}/logs/stream",
            "/api/v1/apps/{app_id}/start",
        ] {
            assert!(paths.contains_key(path), "OpenAPI path missing: {path}");
        }
        // 删除面防复活：releases + rcoder 侧 publish 任务体系路径不得再出现
        // （构建链收敛为 file-server /api/userapp/* 接口族，rcoder 不再做发布编排）
        for gone in [
            "/api/v1/apps/{app_id}/releases/prepare",
            "/api/v1/apps/{app_id}/releases/rollback",
            "/api/v1/apps/{app_id}/releases/{release_id}/activate",
            "/api/v1/apps/{app_id}/build",
            "/api/v1/apps/publish/tasks/query",
            "/api/v1/apps/publish/tasks/{task_id}",
            "/api/v1/apps/publish/tasks/{task_id}/stream",
            "/api/v1/apps/publish/tasks/{task_id}/cancel",
        ] {
            assert!(!paths.contains_key(gone), "deleted path reappeared: {gone}");
        }
    }

    /// file-server 文档聚合进 rcoder Swagger UI（全量剔除
    /// [`INTERNAL_USERAPP_PATHS`] 内部路由面后挂载）。此测试锁定聚合链路活着:
    /// 语义锚点 + 动态下限——逐条路径清单由 file-server 自己的 openapi 测试
    /// （总数 + contains_key）锁定, 这里不重复维护。
    #[test]
    fn file_server_document_covers_userapp_and_project_paths() {
        let document = file_server_document();
        let paths = &document.paths.paths;
        // 锚点: 项目创建入口 + UserApp 打包链 (跨域语义关键路径)
        for path in [
            "/api/project/create-project",
            "/api/userapp/build",
            "/api/userapp/projects/detect",
        ] {
            assert!(
                paths.contains_key(path),
                "file-server OpenAPI path missing: {path}"
            );
        }
        let userapp_count = paths
            .keys()
            .filter(|p| p.starts_with("/api/userapp/"))
            .count();
        // 全量 100 paths - 12 个内部镜像 path(file-server 侧; 第 13 条
        // /api/userapp/workspace 仅存在于 rcoder 侧转发路由, 对 file-server 文档
        // 是永不命中的防御条目) = 88
        assert!(paths.len() >= 85, "聚合文档路径总数异常: {}", paths.len());
        assert!(userapp_count >= 15, "userapp 路径数异常: {userapp_count}");
    }

    /// 内部路由面防回归: 13 个 [`INTERNAL_USERAPP_PATHS`]（file-server-proxy 分流
    /// 代理的上游镜像接口）不得出现在任何一份对外文档；同时保留面锚点仍在
    /// （userApp 公开域: dev 生命周期/编译/打包/任务/静态）。
    #[test]
    fn internal_userapp_paths_are_hidden_from_docs() {
        let primary = primary_document();
        for path in INTERNAL_USERAPP_PATHS {
            assert!(
                !primary.paths.paths.contains_key(path),
                "主文档泄露内部路由: {path}"
            );
        }
        let file_server_doc = file_server_document();
        for path in INTERNAL_USERAPP_PATHS {
            assert!(
                !file_server_doc.paths.paths.contains_key(path),
                "file-server 文档泄露内部路由: {path}"
            );
        }
        // 保留面锚点（不在内部清单的 userApp 公开接口）
        for anchor in [
            "/api/userapp/build",
            "/api/userapp/dev/start",
            "/api/userapp/get-logs",
            "/api/userapp/projects/detect",
        ] {
            assert!(
                primary.paths.paths.contains_key(anchor),
                "主文档缺 userApp 公开路径: {anchor}"
            );
        }
    }

    /// HTTP 层验证：两份 openapi.json 均由 Swagger UI 路由实际提供服务。
    /// 主文档额外验证 userApp 域已合入（默认打开即可见）。
    #[tokio::test]
    async fn swagger_ui_serves_both_documents() {
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let app = Router::new().merge(create_swagger_ui());
        for (path, needle) in [
            ("/api/docs/openapi.json", "/api/v1/apps"),
            ("/api/docs/openapi.json", "/api/userapp/dev/start"),
            ("/api/docs/file-server.json", "/api/userapp/build"),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "GET {path} 非 200");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let text = String::from_utf8_lossy(&body);
            assert!(text.contains(needle), "{path} 响应缺少 {needle}");
        }
    }

    /// Scalar 文档页 HTTP 层验证：两个页面均实际提供服务，spec 内嵌于
    /// HTML（含各文档语义锚点路径）。与 SwaggerUi 同 Router merge 构造
    /// 即验证 `/api/docs/{*rest}` 通配与 `/api/docs/scalar` static 共存
    /// 不冲突（冲突会在 merge 时 panic）。
    #[tokio::test]
    async fn scalar_docs_serve_both_documents() {
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let app = Router::new()
            .merge(create_swagger_ui())
            .merge(create_scalar_docs());
        for (path, needle) in [
            // 主文档页：Scalar 引导脚本 + spec 锚点（应用管理 + userApp）
            ("/api/docs/scalar", "@scalar/api-reference"),
            ("/api/docs/scalar", "/api/v1/apps"),
            ("/api/docs/scalar", "/api/userapp/dev/start"),
            // file-server 页：全量文档锚点
            ("/api/docs/scalar/file-server", "/api/userapp/build"),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "GET {path} 非 200");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let text = String::from_utf8_lossy(&body);
            assert!(text.contains(needle), "{path} 响应缺少 {needle}");
        }
        // Swagger UI 原有路由不受 Scalar 挂载影响（static 优先不改变通配兜底）
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/docs/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "GET /api/docs/ 非 200");
    }

    /// 主文档选择性合入的语义锁定：userApp 域（/api/userapp/*）在、其余
    /// file-server 域（project 等）不在（防主文档膨胀）。
    #[test]
    fn primary_document_merges_userapp_domain_only() {
        let paths = &primary_document().paths.paths;
        for anchor in ["/api/userapp/dev/start", "/api/userapp/build"] {
            assert!(
                paths.contains_key(anchor),
                "主文档缺 userApp 路径: {anchor}"
            );
        }
        assert!(
            !paths.contains_key("/api/project/create-project"),
            "project 域不应合入主文档（留在 file-server.json）"
        );
    }

    /// UserApp 全部对接端点（`/api/v1/apps` + `/userapp/` 代理文档接口）的文档质量
    /// 防回归：
    /// 1. 每个操作必须有非空 summary 或 description（handler `///` doc 注释）；
    /// 2. 成功响应（2xx/3xx——/userapp/ 文档接口的成功码是 307）必须有非空 description；
    /// 3. 必须声明至少一个 4xx/5xx 错误响应（与 handler 实际错误分支对应）。
    ///
    /// 新增 UserApp 端点未写注释会在此失败——样板见 app_manager/handlers（/api/v1/apps 族）。
    #[test]
    fn userapp_openapi_annotations_are_complete() {
        let document = ApiDoc::openapi();
        let mut checked = 0usize;
        for (path, item) in &document.paths.paths {
            // /userapp/routes 速查表是纯静态文档接口（无错误分支），不在质量检查内
            let is_userapp_proxy_doc = ["/userapp/dev/", "/userapp/prod/"]
                .iter()
                .any(|prefix| path.starts_with(prefix));
            if !path.starts_with("/api/v1/apps") && !is_userapp_proxy_doc {
                continue;
            }
            for operation in [&item.get, &item.post].into_iter().flatten() {
                let described = operation
                    .summary
                    .as_ref()
                    .is_some_and(|s| !s.trim().is_empty())
                    || operation
                        .description
                        .as_ref()
                        .is_some_and(|d| !d.trim().is_empty());
                assert!(
                    described,
                    "OpenAPI 操作缺少 doc 注释（summary/description 均为空）: {path}"
                );

                let responses = &operation.responses.responses;
                let success = responses.keys().find_map(|code| {
                    let status = code.trim().parse::<u16>().ok()?;
                    (200..400).contains(&status).then_some(code.clone())
                });
                let success =
                    success.unwrap_or_else(|| panic!("OpenAPI 操作缺少 2xx/3xx 成功响应: {path}"));
                let utoipa::openapi::RefOr::T(ok) = responses
                    .get(&success)
                    .unwrap_or_else(|| panic!("OpenAPI 操作缺少 {success} 响应: {path}"))
                else {
                    panic!("{success} 响应不应为 $ref: {path}")
                };
                assert!(
                    !ok.description.trim().is_empty(),
                    "{success} 响应缺少 description: {path}"
                );

                let has_error_code = responses.keys().any(|code| {
                    code.trim()
                        .parse::<u16>()
                        .is_ok_and(|c| (400..600).contains(&c))
                });
                assert!(
                    has_error_code,
                    "OpenAPI 操作未声明任何 4xx/5xx 错误响应: {path}"
                );
                checked += 1;
            }
        }
        // 覆盖数下限：app_manager 25（create REST 面 + releases 五接口已删）+
        // app_manager /api/v1/apps 族 + /userapp/ 代理文档 6（开发域 ttyd/vnc/audio/ime
        // + 运行容器 ttyd/pgweb）+ dbx 两阶段代理文档 2（dev/prod）。
        assert!(
            checked >= 33,
            "UserApp OpenAPI 端点覆盖数异常偏少: {checked}"
        );
    }

    /// 文档质量防回归：`/computer/pod/*` 接口的 GET 参数与 POST 请求体 DTO 字段
    /// 必须有非空 description（doc comment 是 swagger description 唯一来源，
    /// 同事看 swagger 对接；pod 域此前零字段级覆盖，userApp 三字段形态
    /// service_type=userapp/app_id/app_stage 靠此测试守卫）。
    #[test]
    fn pod_endpoints_fields_are_documented() {
        let document = ApiDoc::openapi();
        let mut checked_params = 0usize;
        let mut checked_fields = 0usize;
        for (path, item) in &document.paths.paths {
            if !path.starts_with("/computer/pod/") {
                continue;
            }
            for operation in [&item.get, &item.post].into_iter().flatten() {
                if let Some(params) = &operation.parameters {
                    for p in params {
                        assert!(
                            p.description.as_ref().is_some_and(|d| !d.trim().is_empty()),
                            "{path} 参数 {:?} 缺少 description（补 doc comment）",
                            p.name
                        );
                        checked_params += 1;
                    }
                }
            }
        }
        // 请求体 DTO（POST 三兄弟）的 schema 字段（$ref 字段如 resource_limits 跳过）
        for (name, schema) in &document
            .components
            .as_ref()
            .expect("components present")
            .schemas
        {
            if !matches!(
                name.as_str(),
                "EnsurePodRequest" | "KeepalivePodRequest" | "RestartPodRequest"
            ) {
                continue;
            }
            let utoipa::openapi::RefOr::T(utoipa::openapi::schema::Schema::Object(obj)) = schema
            else {
                continue;
            };
            for (field, value) in &obj.properties {
                let utoipa::openapi::RefOr::T(utoipa::openapi::schema::Schema::Object(field_obj)) =
                    value
                else {
                    continue;
                };
                assert!(
                    field_obj
                        .description
                        .as_ref()
                        .is_some_and(|d| !d.trim().is_empty()),
                    "schema {name} 字段 {field} 缺少 description（补 doc comment）"
                );
                checked_fields += 1;
            }
        }
        // 动态下限防空转：GET status/vnc-status 各 9 参数（含新增 app_id/app_stage）；
        // 三个 POST DTO 各 9 个内联字段（resource_limits 为 $ref 不计）。
        assert!(
            checked_params >= 16,
            "pod 接口参数覆盖数异常偏少: {checked_params}"
        );
        assert!(
            checked_fields >= 24,
            "pod 请求体字段覆盖数异常偏少: {checked_fields}"
        );
    }
}
