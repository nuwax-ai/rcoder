// router 整体由 binary (main.rs) 使用，lib 内不直接调用 create_router / ApiDoc 等。
// 抑制 dead_code 以避免 lib 维度误报。
#![allow(dead_code)]

use arc_swap::ArcSwap;
use container_runtime_api::ContainerRuntime;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::Request,
    middleware::Next,
    response::IntoResponse,
    response::Response,
    routing::{get, post},
};
use serde::Serialize;
use shared_types::ProjectAndContainerInfo;

use crate::{
    config::{ApiKeyAuthConfig, AppConfig},
    handler,
    storage::ProjectAdapter,
};
use agent_provisioning::AgentDownloadManager;
use rcoder_telemetry::{HttpMetricsLayer, TelemetryGuard};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

async fn locale_context_middleware(mut req: Request<axum::body::Body>, next: Next) -> Response {
    let locale = shared_types::parse_accept_language(
        req.headers()
            .get("accept-language")
            .and_then(|v| v.to_str().ok()),
    );

    req.extensions_mut().insert(locale);

    shared_types::scope_request_locale(locale, async move { next.run(req).await }).await
}

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
    /// 项目适配器 - 纯 DashMap 内存存储 + RAII 自动资源回收
    ///
    /// 使用 `Arc<ProjectAdapter>` 共享同一实例：
    /// - AppState 业务逻辑通过 `state.projects.method()` 访问（Arc 自动 deref）
    /// - 同一 `Arc` 作为 `Arc<dyn ContainerLookup>` 注入 Pingora 代理层，
    ///   使 /web/ttyd、/computer/ttyd 等路由能解析容器 IP
    ///   （DashMap 的 clone 是深拷贝，必须共享同一 Arc 实例才能保证数据一致）
    pub projects: Arc<ProjectAdapter>,
    /// Pingora 代理服务引用（用于读取真实指标）
    pub pingora_service: Option<Arc<rcoder_proxy::PingoraProxyService>>,
    /// gRPC 连接池（用于与 agent_runner 通信）
    pub grpc_pool: Arc<crate::grpc::GrpcChannelPool>,
    /// Session 级共享 SSE 流注册表（每 session 一条 agent_runner SubscribeProgress 流，
    /// 多 HTTP SSE 客户端 fan-out 共享，消除重复推送；配合 agent_runner 的 seq 增量 replay）
    pub session_stream_registry: Arc<crate::grpc::SessionStreamRegistry>,
    /// 🆕 可热更新的 API Key 配置（使用 ArcSwap 实现无锁读取）
    pub api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
    /// 🆕 容器创建中标记: user_id -> 创建开始时间
    /// 用于防止并发 pod_ensure 请求互相干扰（无锁方案）
    pub pod_creating: Arc<dashmap::DashMap<String, std::time::Instant>>,
    /// 🚀 容器创建完成通知通道（替代轮询等待）
    /// 当容器创建完成时，发送 user_id 通知等待方
    pub pod_created_tx: Arc<broadcast::Sender<String>>,
    /// 🆕 容器前缀（从配置读取，启动时初始化）
    pub container_prefix_rcoder: String,
    pub container_prefix_computer: String,
    /// 容器运行时（通过 DI 注入，替代全局 RuntimeManager::get()）
    pub runtime: Arc<dyn ContainerRuntime>,
    /// RAII 资源回收器接收端（在 start_cleanup_task 中取出并启动 ResourceReaper）
    pub cleanup_rx: Arc<
        std::sync::Mutex<
            Option<tokio::sync::mpsc::UnboundedReceiver<crate::storage::CleanupRequest>>,
        >,
    >,
    /// Agent 下载管理器（统一缓存）
    pub agent_download_manager: Arc<AgentDownloadManager>,
    /// 应用管理服务
    pub app_service: Arc<dyn app_manager::AppServiceTrait>,
    /// UserApp 活动状态注册表（闲置回收/流量唤醒共享状态；扫描器读 last_accessed/waking）
    pub activity: Arc<app_manager::AppActivityRegistry>,
    /// K8s 集群域名（用于构建 K8s Service FQDN）
    pub cluster_domain: String,
    /// UserApp 自动化构建发布任务表(rcoder 侧编排:正向调 agent-runner build + 同进程 app_manager 发布)。
    pub publish_tasks: Arc<crate::userapp_publish::PublishTaskStore>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        config: AppConfig,
        pingora: Option<Arc<rcoder_proxy::PingoraProxyService>>,
        api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
        container_prefix_rcoder: String,
        container_prefix_computer: String,
        runtime: Arc<dyn ContainerRuntime>,
        projects: Arc<ProjectAdapter>,
        cleanup_rx: tokio::sync::mpsc::UnboundedReceiver<crate::storage::CleanupRequest>,
        activity: Arc<app_manager::AppActivityRegistry>,
    ) -> anyhow::Result<Self> {
        // ProjectAdapter 由调用方（main.rs）提前创建并注入，
        // 以便同一 Arc 实例可同时作为 Arc<dyn ContainerLookup> 注入 Pingora 代理层。
        let cluster_domain = shared_types::get_k8s_cluster_domain();

        // 创建容器创建完成通知通道（缓冲区大小 32，足够应对并发创建）
        let (pod_created_tx, _) = broadcast::channel(32);

        // 初始化 Agent 下载管理器
        let cache_dir = std::env::var("AGENT_CACHE_DIR")
            .unwrap_or_else(|_| shared_types::AGENT_CACHE_DIR.to_string());
        let agent_download_manager =
            Arc::new(AgentDownloadManager::new(cache_dir).map_err(|e| {
                anyhow::anyhow!("failed to initialize agent download manager: {}", e)
            })?);

        // 初始化应用管理服务（Docker / K8s 统一构造，运行时由 access_mode 决定行为）
        let app_service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(
            app_manager::service::AppService::new(
                config.app_manager.clone(),
                runtime.clone(),
                activity.clone(),
                pingora.clone(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to initialize app service: {}", e))?,
        );

        Ok(Self {
            config,
            projects,
            pingora_service: pingora,
            grpc_pool: Arc::new(crate::grpc::GrpcChannelPool::new()),
            session_stream_registry: Arc::new(crate::grpc::SessionStreamRegistry::new()),
            api_key_config,
            pod_creating: Arc::new(DashMap::new()),
            pod_created_tx: Arc::new(pod_created_tx),
            container_prefix_rcoder,
            container_prefix_computer,
            runtime,
            cleanup_rx: Arc::new(std::sync::Mutex::new(Some(cleanup_rx))),
            agent_download_manager,
            app_service,
            activity,
            cluster_domain,
            publish_tasks: Arc::new(crate::userapp_publish::PublishTaskStore::new()),
        })
    }

    /// 获取容器运行时引用（替代 RuntimeManager::get()）
    #[inline]
    pub fn runtime(&self) -> &Arc<dyn ContainerRuntime> {
        &self.runtime
    }

    // ========== 向后兼容的便捷方法 ==========

    /// 获取项目信息（替代 project_and_agent_map.get）
    #[inline]
    pub fn get_project(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        self.projects.get(project_id)
    }

    /// 插入项目信息（替代 project_and_agent_map.insert）
    ///
    /// # Errors
    /// 如果 `service_type` 未设置，透传错误。
    #[inline]
    pub fn insert_project(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
    ) -> anyhow::Result<()> {
        self.projects.insert(project_id.clone(), info)
    }

    /// 插入项目并设置 session 映射（单次原子写入，消除 CAS 竞态）
    ///
    /// # Errors
    /// 如果 `service_type` 未设置，透传错误。
    #[inline]
    pub fn insert_project_with_session(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: &str,
    ) -> anyhow::Result<()> {
        self.projects
            .insert_with_session(project_id, info, Some(session_id))
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

    /// 通过会话ID获取容器名称（用于容器重启后的容器查询）
    ///
    /// 与 `get_container_id_by_session` 不同，返回稳定的 `container_name`。
    /// 即使容器被重建，container_name 保持不变，可直接通过 Docker API 查询容器状态。
    #[inline]
    pub fn get_container_name_by_session(&self, session_id: &str) -> Option<String> {
        self.projects.get_container_name_by_session(session_id)
    }

    /// 向已有 project 追加 session（C1 修复推荐路径，多 session 模型）
    ///
    /// 单步原子，多 session 并存。返回 false 表示 project 不存在。
    #[inline]
    pub fn add_session_to_project(&self, project_id: &str, session_id: &str) -> bool {
        self.projects.add_session_to_project(project_id, session_id)
    }

    /// 更新会话信息（已废弃，请用 `add_session_to_project` 或 `insert_project_with_session`）
    #[inline]
    #[deprecated(
        since = "0.0.0",
        note = "非原子，请用 `add_session_to_project` 走多 session 路径"
    )]
    #[allow(deprecated)]
    pub fn update_session(&self, project_id: &str, session_id: &str) {
        self.projects.update_session(project_id, session_id);
    }

    /// 原子更新会话信息（已废弃，多 session 模型下 CAS 语义不再适用）
    #[inline]
    #[deprecated(since = "0.0.0", note = "CAS 语义在多 session 模型下不再适用")]
    #[allow(dead_code, deprecated)]
    pub fn update_session_atomic(
        &self,
        project_id: &str,
        new_session_id: &str,
        expected_current_session_id: Option<&str>,
    ) -> bool {
        self.projects
            .update_session_atomic(project_id, new_session_id, expected_current_session_id)
    }

    /// 清除会话信息（清所有 session，agent stop 场景）
    #[inline]
    pub fn clear_session(&self, project_id: &str) {
        self.projects.clear_session(project_id);
    }

    /// 清除单个 session（保留 project 的其他 session）
    #[inline]
    pub fn clear_session_one(&self, project_id: &str, session_id: &str) -> bool {
        self.projects.clear_session_one(project_id, session_id)
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

/// 内部 API 路由（供 rcoder-gateway 调用）
///
/// 这些端点挂载在中间件之前，绕过 API Key 鉴权。
fn create_internal_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/internal/pod/ensure", post(handler::internal_pod_ensure))
        .route(
            "/internal/session/{session_id}/resolve",
            get(handler::internal_session_resolve),
        )
        .with_state(state)
}

/// 创建 Axum 路由
pub fn create_router(state: Arc<AppState>, telemetry: Option<Arc<TelemetryGuard>>) -> Router {
    let api_routes = Router::new()
        .route("/chat", post(handler::handle_chat))
        // Axum SSE 代理处理器，直接返回 SSE 流
        .route(
            "/agent/progress/{session_id}",
            get(handler::agent_session_notification),
        )
        .route("/agent/session/cancel", post(handler::agent_session_cancel))
        .route(
            "/agent/notify-resolved",
            post(handler::agent_notify_resolved),
        )
        .route("/agent/stop", post(handler::agent_stop))
        .route("/agent/status/{project_id}", get(handler::agent_status))
        .with_state(state.clone());

    // Computer Agent Runner 路由
    let computer_routes = Router::new()
        .route("/computer/chat", post(handler::handle_computer_chat))
        .route("/computer/agent/stop", post(handler::computer_agent_stop))
        .route(
            "/computer/agent/status",
            post(handler::computer_agent_status),
        ) // 🆕 新增
        .route(
            "/computer/agent/session/cancel",
            post(handler::computer_agent_session_cancel),
        )
        .route(
            "/computer/notify-resolved",
            post(handler::computer_notify_resolved),
        )
        // 进度流复用现有的 agent_session_notification
        .route(
            "/computer/progress/{session_id}",
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
        // 🆕 音频代理路由（用于 OpenAPI 文档）
        .route(
            "/computer/audio/{user_id}/{project_id}/{*path}",
            get(handler::computer_audio_proxy),
        )
        // 🆕 IME 代理路由（用于 OpenAPI 文档）
        .route(
            "/computer/ime/{user_id}/{project_id}/{*path}",
            get(handler::computer_ime_proxy),
        )
        // 🆕 Computer Agent-runner 容器 PG 管理（重置密码 / 建库; rcoder exec 容器内 psql）
        .route(
            "/computer/db/{user_id}/reset-password",
            post(handler::computer_db_reset_password),
        )
        .route(
            "/computer/db/{user_id}/create-database",
            post(handler::computer_db_create_database),
        )
        .with_state(state.clone());

    // Pingora 代理 API 路由（用于文档和状态查询）
    let proxy_api_routes = Router::new()
        .route("/proxy/status", get(handler::proxy_status))
        .route("/proxy/stats", get(handler::proxy_stats))
        .route("/proxy/config", get(handler::proxy_config))
        .with_state(state.clone());

    // DevComputer 调试路由 — 委托给 /computer/* 处理器，共享同一个容器
    let devcomputer_routes = Router::new()
        .route("/devcomputer/chat", post(handler::handle_devcomputer_chat))
        .route(
            "/devcomputer/agent/stop",
            post(handler::devcomputer_agent_stop),
        )
        .route(
            "/devcomputer/agent/status",
            post(handler::devcomputer_agent_status),
        )
        .route(
            "/devcomputer/agent/session/cancel",
            post(handler::devcomputer_agent_session_cancel),
        )
        .route(
            "/devcomputer/notify-resolved",
            post(handler::devcomputer_notify_resolved),
        )
        .route(
            "/devcomputer/progress/{session_id}",
            get(handler::devcomputer_agent_progress_notification),
        )
        .with_state(state.clone());

    // 调试路由（仅用于开发和问题排查，需要 feature flag "debug" 启用）
    #[cfg(feature = "debug")]
    let debug_routes = Router::new()
        .route("/debug/sql", get(handler::debug_dump_summary))
        .route("/debug/projects", get(handler::debug_list_projects))
        .route("/debug/containers", get(handler::debug_list_containers))
        .route("/debug/storage/stats", get(handler::debug_storage_stats))
        .with_state(state.clone());

    // 健康检查路由
    let health_routes = Router::new()
        .route("/health", get(handler::health_check))
        .with_state(state.clone());

    // P0-5: Agent Management 路由(全部 POST + body 解析)
    // - 简单 JSON 端点使用 I18nJsonOrQuery(同时支持 JSON body 和 ?project_id=xxx query)
    // - install 端点使用 multipart/form-data(file + metadata JSON 字段)
    //
    // ⚠️ install 路由的 body 限制必须在 Router 层挂,而不是 MethodRouter 层。
    // axum 的 `Multipart` 提取器通过 `with_limited_body()` 读取
    // `DefaultBodyLimitKind` 扩展(Request 上挂的 layer 才生效),`MethodRouter::layer`
    // 出来的 MethodRouter 不携带这个扩展,无法被 multipart 识别。
    // 此外 `RequestBodyLimitLayer` 是 tower 中间件,只读取 Content-Length 头,
    // 对 streaming 的 multipart body 不直接生效,但保留作为 defense-in-depth。
    let install_route = Router::new()
        .route("/agent-mgmt/agents/install", post(handler::install_agent))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            1024 * 1024 * 1024,
        ));

    let agent_mgmt_routes = Router::new()
        .route("/agent-mgmt/agents/list", post(handler::list_agents))
        .route("/agent-mgmt/agents/get", post(handler::get_agent))
        .route("/agent-mgmt/agents/check", post(handler::check_agent))
        .merge(install_route)
        .route(
            "/agent-mgmt/agents/install-from-url",
            post(handler::install_from_url),
        )
        .route(
            "/agent-mgmt/agents/install-from-npm",
            post(handler::install_from_npm),
        )
        .route(
            "/agent-mgmt/agents/uninstall",
            post(handler::uninstall_agent),
        )
        .with_state(state.clone());

    // 应用管理路由
    let app_manager_state = Arc::new(app_manager::handlers::AppManagerState {
        app_service: state.app_service.clone(),
    });
    let app_manager_routes = app_manager::routes::app_manager_routes()
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024)) // 1GiB（upload 压缩包，覆盖全局 50MB）
        .with_state(app_manager_state);

    // UserApp 自动化构建发布(rcoder 侧编排):publish/build + task 查询/SSE/cancel
    let userapp_publish_routes =
        crate::userapp_publish::handler::routes().with_state(state.clone());

    let mut router = Router::new()
        .merge(health_routes)
        .merge(api_routes)
        .merge(computer_routes)
        .merge(devcomputer_routes)
        .merge(proxy_api_routes)
        .merge(agent_mgmt_routes)
        .merge(app_manager_routes)
        .merge(userapp_publish_routes);

    // 仅在启用 debug feature 时添加调试路由
    #[cfg(feature = "debug")]
    {
        router = router.merge(debug_routes);
    }

    // 添加 /metrics 端点（如果启用了 Prometheus）
    if let Some(ref guard) = telemetry {
        let guard_clone = Arc::clone(guard);
        router = router.route(
            "/metrics",
            get(move || {
                let guard = Arc::clone(&guard_clone);
                async move { metrics_handler(guard).await }
            }),
        );
    }

    // 🆕 克隆共享的 API Key 配置用于中间件
    let api_key_config = Arc::clone(&state.api_key_config);

    router
        .merge(create_swagger_ui())
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024)) // 50MB body 大小限制
        .layer(HttpMetricsLayer::new()) // HTTP 指标中间件
        // API Key 鉴权中间件（支持热更新）
        .layer(axum::middleware::from_fn(move |req, next| {
            crate::middleware::api_key_middleware::api_key_middleware_handler(
                Arc::clone(&api_key_config),
                req,
                next,
            )
        }))
        .layer(axum::middleware::from_fn(locale_context_middleware))
        // 内部 API（供 rcoder-gateway 调用，绕过 API Key 鉴权）
        .merge(create_internal_routes(state))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-frame-options"),
            axum::http::HeaderValue::from_static("DENY"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-content-type-options"),
            axum::http::HeaderValue::from_static("nosniff"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("strict-transport-security"),
            axum::http::HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("referrer-policy"),
            axum::http::HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-xss-protection"),
            axum::http::HeaderValue::from_static("0"),
        ))
}

/// Prometheus 指标处理器
async fn metrics_handler(telemetry: Arc<TelemetryGuard>) -> impl IntoResponse {
    match telemetry.render_metrics() {
        Some(metrics) => (
            axum::http::StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            metrics,
        ),
        None => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            "Prometheus metrics not enabled".to_string(),
        ),
    }
}

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
        handler::pod_vnc_status,
        // Pingora 代理接口
        handler::proxy_status,
        handler::proxy_stats,
        handler::proxy_config,
        handler::proxy_to_port,
        handler::proxy_to_port_with_path,
        handler::proxy_to_app_with_path,
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
        app_manager::handlers::create_app,
        app_manager::handlers::query_apps,
        app_manager::handlers::get_app,
        app_manager::handlers::update_app,
        app_manager::handlers::delete_app,
        app_manager::handlers::start_app,
        app_manager::handlers::stop_app,
        app_manager::handlers::restart_app,
        app_manager::handlers::set_recycle_policy,
        app_manager::handlers::get_app_logs,
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
        app_manager::handlers::get_app_file_logs,
    ),
    components(
        schemas(
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
            app_manager::models::CreateAppRequest,
            app_manager::models::AppInfo,
            app_manager::models::AppRuntimeInfo,
            app_manager::models::AppStatus,
            app_manager::models::QueryAppsRequest,
            app_manager::models::UpdateAppRequest,
            app_manager::models::LogParams,
            app_manager::models::LogEntry,
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

/// 创建 Swagger UI 路由
pub fn create_swagger_ui() -> SwaggerUi {
    SwaggerUi::new("/api/docs")
        .url("/api/docs/openapi.json", ApiDoc::openapi())
        .config(utoipa_swagger_ui::Config::new(["/api/docs/openapi.json"]))
}
