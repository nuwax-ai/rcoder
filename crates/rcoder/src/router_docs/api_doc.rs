//! rcoder 主文档的 utoipa 声明（paths / components / tags 全量枚举）。
//!
//! 从 router_docs.rs 目录化拆出：纯宏声明零逻辑；聚合装配与文档 UI 在
//! [`super`]（create_swagger_ui / create_scalar_docs）。

use utoipa::OpenApi;

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
        handler::pod_stop,
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
        app_manager::handlers::stream_app_logs_v1,
        app_manager::handlers::upload_from_url,
        crate::userapp_forward::forward::flat_dev_projects_detect,
        crate::userapp_forward::forward::flat_dev_projects_confirm,
        crate::userapp_forward::forward::flat_dev_install_project,
        crate::userapp_forward::db::reset_password,
        crate::userapp_forward::db::create_database,
    ),
    components(
        schemas(
            // userApp 转发层（PG 账号/库管理；create-workspace 为内部接口不入文档）
            shared_types::UserappDbResetPasswordRequest,
            shared_types::UserappDbCreateDatabaseRequest,
            // 日志域 wire DTO（与容器内 app-cli 同源；logs/sources/query + logs/query 响应面）
            shared_types::LogQueryRequest,
            shared_types::LogSelector,
            shared_types::LogSourceInfo,
            shared_types::LogRecord,
            shared_types::SourceError,
            shared_types::LogQueryResponse,
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
            handler::StopPodRequest,
            handler::StopPodResponse,
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
        (name = "Userapp · dev · 构建任务", description = "dev 专属（目标容器恒为 UserappBuilder 开发容器）：构建触发、任务查询/取消与进度 SSE、制品包下载"),
        (name = "Userapp · dev · 工作区与工具链", description = "dev 专属（目标容器 UserappBuilder 开发容器）：workspace 创建、命令执行、打包下载、模板与技能安装、项目类型探测确认"),
        (name = "Userapp · dev · 进程管理", description = "dev 专属（目标容器 UserappBuilder 开发容器，路径自带 dev）：dev server 进程启停/列表/日志"),
        (name = "Userapp · dev · 终端工具", description = "dev 专属（目标容器 UserappBuilder 开发容器）：ttyd/VNC/audio/ime/dbx 开发终端入口"),
        (name = "Userapp · prod · 部署与启停", description = "prod 专属（目标为 Userapp 生产运行容器/Deployment）：部署启动、停止、重启与配置更新"),
        (name = "Userapp · prod · 应用查询", description = "prod 专属：应用详情、分页查询与运行时清单"),
        (name = "Userapp · prod · 终端工具", description = "prod 专属（目标容器 Userapp 生产运行容器）：ttyd/dbx 生产终端入口"),
        (name = "Userapp · 双态 · 文件与存储", description = "dev/prod 双态（路径 {app_id}/{app_stage} 段分派）：文件上传/管理与存储卷查询/清理/销毁"),
        (name = "Userapp · 双态 · 日志", description = "dev/prod 双态（路径 {app_stage} 段分派）：日志源、检索与 SSE 实时流，转发容器内 app-cli"),
        (name = "Userapp · 双态 · 数据库", description = "dev/prod 双态（路径 {app_stage} 段分派）：应用 PostgreSQL 改密/建库（凭据对齐内嵌 start 部署链）"),
        (name = "Userapp · 双态 · 生命周期", description = "dev/prod 双态（路径 {app_stage} 段分派）：健康/统计/事件、回收策略与删除"),
        (name = "Userapp · 访问入口", description = "流量代理（/api/v1/userapp/proxy/app/{dev,prod}，前端切换只改 dev→prod 一段）与终端代理路由速查表"),
        (name = "computer", description = "Computer Agent 桌面、聊天与容器内 PG 管理接口；chat 与 agent 族（status/stop/cancel/notify-resolved/cache/clean）支持 service_type=userapp + app_id 分派（仅 dev 阶段）"),
        (name = "pod", description = "Pod 容器管理接口（监控/保活/重启；支持 service_type=userapp 分派 dev/prod 容器）"),
        (name = "proxy", description = "Pingora 反向代理接口，支持端口路由和负载均衡"),
        (name = "chat", description = "AI 聊天对话接口，支持多媒体内容"),
        (name = "agent", description = "AI 代理会话管理和实时通知接口"),
        (name = "devcomputer", description = "DevComputer 调试接口（与 /computer 共享容器，自动注入 auto_reload 配置）"),
        (name = "agent-mgmt", description = "Agent 二进制安装/卸载/检查接口(P0-4: rcoder 转发到 agent_runner 容器)"),
        (name = "system", description = "系统健康检查和状态监控接口"),
    ),
    info(
        description = r#"
RCoder AI 服务 API

基于 ACP (Agent Client Protocol) 的 AI 驱动开发平台，提供完整的 AI 代理集成解决方案。

## 主要功能

- **Userapp 应用引擎**: 无状态应用 Pod 的完整生命周期（开发构建 → 部署发布 → 终端/代理访问），按 `Userapp ·` 前缀环境维度分组（dev / prod / 双态 / 访问入口）
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
