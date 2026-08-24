//! OpenAPI 文档聚合与 Swagger UI（从 router.rs 拆出；路由组装仍在 router.rs）。
//!
//! [`ApiDoc`] 是 rcoder 主文档的 utoipa 声明（paths/components 全量），
//! [`create_swagger_ui`] 聚合两份文档（主文档 + file-server 全量文档，
//! UI 顶部下拉切换）。

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
        app_manager::handlers::prepare_release,
        app_manager::handlers::activate_release,
        app_manager::handlers::rollback_release,
        app_manager::handlers::list_releases,
        app_manager::handlers::delete_release,
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
        crate::userapp_publish::handler::build,
        crate::userapp_publish::handler::query_tasks,
        crate::userapp_publish::handler::get_task,
        crate::userapp_publish::handler::stream_task,
        crate::userapp_publish::handler::cancel_task,
        crate::userapp_forward::workspace::create_workspace,
        crate::userapp_forward::db::align_credentials,
    ),
    components(
        schemas(
            // userApp 转发层（create-workspace + PG 凭据对齐）
            crate::userapp_forward::workspace::CreateWorkspaceBody,
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
            crate::userapp_publish::models::PublishBody,
            crate::userapp_publish::models::QueryPublishTasksRequest,
            crate::userapp_publish::models::PublishTaskFilters,
            crate::userapp_publish::PublishTaskKind,
            crate::userapp_publish::PublishTaskStatus,
            crate::userapp_publish::PublishTaskSnapshot,
            app_manager::models::PaginatedResponse<crate::userapp_publish::PublishTaskSnapshot>,
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
/// 聚合两份文档（UI 顶部下拉切换）：rcoder 主文档 + file-server 文档。
/// file-server 全量文档（含 /api/userapp）始终聚合在此；实际路由宿主：
/// 老路径（project/computer/git/build）常驻 rcoder 主服务（`merged_router`），
/// userapp 域由 rcoder 转发层接管、本地实现在 per-app 开发容器内 file-server（60000）。
///
/// 主文档额外合入 userApp 业务域（file-server 的 `/api/userapp/*` 路径 +
/// schemas）——Swagger 默认打开主文档即见 userApp 全貌（dev 生命周期/编译/
/// 文件/静态），无需切下拉；其余 file-server 域（project/computer/git 等）
/// 仍只在 file-server.json，防主文档膨胀。
pub fn create_swagger_ui() -> SwaggerUi {
    SwaggerUi::new("/api/docs")
        .url("/api/docs/openapi.json", primary_document())
        .url(
            "/api/docs/file-server.json",
            file_server::openapi::document(file_server::routes::api_router().into_openapi()),
        )
        .config(utoipa_swagger_ui::Config::new([
            "/api/docs/openapi.json",
            "/api/docs/file-server.json",
        ]))
}

/// 主文档 = rcoder 应用管理 + userApp 业务域（选择性合入）。
fn primary_document() -> utoipa::openapi::OpenApi {
    let mut doc = ApiDoc::openapi();
    let mut userapp =
        file_server::openapi::document(file_server::routes::api_router().into_openapi());
    userapp
        .paths
        .paths
        .retain(|path, _| path.starts_with("/api/userapp"));
    doc.merge(userapp);
    doc
}

#[cfg(test)]
mod openapi_tests {
    use super::*;
    use axum::Router;

    #[test]
    fn userapp_release_log_and_publish_paths_are_documented() {
        let document = ApiDoc::openapi();
        let paths = document.paths.paths;
        for path in [
            "/api/v1/apps/{app_id}/releases/prepare",
            "/api/v1/apps/{app_id}/releases/rollback",
            "/api/v1/apps/{app_id}/logs/query",
            "/api/v1/apps/{app_id}/logs/stream",
            "/api/v1/apps/{app_id}/build",
            "/api/v1/apps/publish/tasks/query",
            "/api/v1/apps/publish/tasks/{task_id}/stream",
        ] {
            assert!(paths.contains_key(path), "OpenAPI path missing: {path}");
        }
    }

    /// file-server 文档**全量**聚合进 rcoder Swagger UI（`create_swagger_ui` 挂完整
    /// openapi.json, 无裁剪）。此测试锁定聚合链路活着: 语义锚点 + 动态下限——
    /// 逐条路径清单由 file-server 自己的 openapi 测试（总数 + contains_key）锁定,
    /// 这里不重复维护; file-server 增删接口时下限断言自动跟随。
    #[test]
    fn file_server_document_covers_userapp_and_project_paths() {
        let document =
            file_server::openapi::document(file_server::routes::api_router().into_openapi());
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
        assert!(paths.len() >= 90, "聚合文档路径总数异常: {}", paths.len());
        assert!(userapp_count >= 20, "userapp 路径数异常: {userapp_count}");
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
    /// 新增 UserApp 端点未写注释会在此失败——样板见 userapp_publish/handler.rs。
    #[test]
    fn userapp_openapi_annotations_are_complete() {
        let document = ApiDoc::openapi();
        let mut checked = 0usize;
        for (path, item) in &document.paths.paths {
            // /userapp/routes 速查表是纯静态文档接口（无错误分支），不在质量检查内
            let is_userapp_proxy_doc = [
                "/userapp/ttyd",
                "/userapp/vnc",
                "/userapp/audio",
                "/userapp/ime",
                "/userapp/pgweb",
            ]
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
        // 覆盖数下限：app_manager 30（删 create REST 面）+ userapp_publish 6 端点
        // + /userapp/ 代理文档 6（开发域 ttyd/vnc/audio/ime + 运行容器 ttyd/pgweb）。
        assert!(
            checked >= 41,
            "UserApp OpenAPI 端点覆盖数异常偏少: {checked}"
        );
    }
}
