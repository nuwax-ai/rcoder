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
    storage::ProjectStoreBackend,
};
use agent_provisioning::AgentDownloadManager;
use rcoder_telemetry::{HttpMetricsLayer, TelemetryGuard};

// 存储契约 trait 引入作用域：AppState.projects 为枚举 ProjectStoreBackend，
// 其上的 get/insert/... 方法均经 shared_types::ProjectStore 解析。
use shared_types::ProjectStore as _;

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
    /// 使用 `Arc<ProjectStoreBackend>` 共享同一实例：
    /// - AppState 业务逻辑通过 `state.projects.method()` 访问（Arc 自动 deref，
    ///   方法经 ProjectStore trait 静态分发到 Memory/Postgres 后端）
    /// - 同一 `Arc` 作为 `Arc<dyn ContainerLookup>` 注入 Pingora 代理层，
    ///   使 /web/ttyd、/computer/ttyd 等路由能解析容器 IP
    ///   （DashMap 的 clone 是深拷贝，必须共享同一 Arc 实例才能保证数据一致）
    pub projects: Arc<ProjectStoreBackend>,
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
    pub pod_creating: Arc<DashMap<String, std::time::Instant>>,
    /// 🚀 容器创建完成通知通道（替代轮询等待）
    /// 当容器创建完成时，发送 user_id 通知等待方
    pub pod_created_tx: Arc<broadcast::Sender<String>>,
    /// 🆕 容器前缀（从配置读取，启动时初始化）
    pub container_prefix_rcoder: String,
    pub container_prefix_computer: String,
    /// 容器运行时（通过 DI 注入，替代全局 RuntimeManager::get()）
    pub runtime: Arc<dyn ContainerRuntime>,
    /// RAII 资源回收器接收端（在 start_cleanup_task 中取出并启动 ResourceReaper）
    pub cleanup_rx:
        Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<crate::storage::CleanupRequest>>>>,
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
        projects: Arc<ProjectStoreBackend>,
        cleanup_rx: tokio::sync::mpsc::Receiver<crate::storage::CleanupRequest>,
        activity: Arc<app_manager::AppActivityRegistry>,
        publish_repo: Option<Arc<dyn rcoder_storage::publish_repo::PublishTaskPersistence>>,
    ) -> anyhow::Result<Self> {
        // 存储后端（Memory/Postgres 枚举）由调用方（main.rs）按配置构造并注入，
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
        let app_service_instance = app_manager::service::AppService::new(
            config.app_manager.clone(),
            runtime.clone(),
            activity.clone(),
            pingora.clone(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("failed to initialize app service: {}", e))?;

        // UserApp 开发资源回收回调（app purge 时回收 UserAppBuilder 开发容器 +
        // per-app PVC；app_manager 的 runtime 视图无 agent 能力，经契约委托本进程）
        app_service_instance.set_dev_cleanup(Arc::new(
            crate::userapp_publish::agent_runner::UserappDevResourcesCleanup::new(
                runtime.clone(),
                projects.clone(),
            ),
        ));

        // P3：PG 模式的应用业务元数据持久化（query name/created_at 过滤数据源）。
        // 装配在 AppService 构造后（内存 cache 在其内部）；load 失败阻断启动——
        // 过滤数据缺失会让 query 语义静默漂移，宁可 Fail Fast。
        if projects.is_postgres() {
            #[cfg(feature = "rcoder-pg")]
            {
                let ProjectStoreBackend::Postgres(store) = &*projects else {
                    unreachable!("is_postgres 为真的分支");
                };
                let metadata_persistence: Arc<dyn shared_types::AppMetadataPersistence> = Arc::new(
                    rcoder_storage::pg::userapp::metadata::PgAppMetadataPersistence::new(
                        store.pool().clone(),
                    ),
                );
                match metadata_persistence.load_all().await {
                    Ok(rows) => {
                        app_service_instance.set_metadata_persistence(metadata_persistence);
                        app_service_instance.apply_metadata_loaded(rows);
                    }
                    Err(e) => {
                        anyhow::bail!("[STORAGE_PG] userapp_metadata load failed: {e:#}");
                    }
                }
            }
        }
        let app_service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(app_service_instance);

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
            publish_tasks: Arc::new(match publish_repo {
                Some(repo) => crate::userapp_publish::PublishTaskStore::with_repo(repo),
                None => crate::userapp_publish::PublishTaskStore::new(),
            }),
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

    /// 关闭某 project 关联的所有 SSE 共享流（容器销毁/项目删除前调用）。
    ///
    /// 必须在 [`remove_project`] 之前调用——remove_project 会清空 project 的 sessions 集合，
    /// 之后无法再据此枚举。销毁语义：让后台 gRPC task 尽快退出，避免对已失效地址重试。
    pub fn shutdown_sse_streams_for_project(&self, project_id: &str) {
        let sids: Vec<String> = self
            .get_project(project_id)
            .map(|info| info.sessions().into_iter().collect())
            .unwrap_or_default();
        for sid in sids {
            if self.session_stream_registry.shutdown_session(&sid) {
                tracing::info!(
                    "[STATE] shutdown SSE stream on project removal: project_id={}, session_id={}",
                    project_id,
                    sid
                );
            }
        }
    }

    /// 按 grpc_addr 关闭关联的所有 SSE 共享流。
    ///
    /// 适用于记录可能已被清空的销毁路径（reaper/restart/ensure/destroyer）：这些路径中
    /// project/session 记录可能在关闭前已被移除，无法再走 [`shutdown_sse_streams_for_project`]，
    /// 但它们都能构造出 grpc_addr（与 `grpc_pool.remove` 同源）。幂等：重复调用安全。
    pub fn shutdown_sse_streams_by_addr(&self, grpc_addr: &str) {
        let closed = self
            .session_stream_registry
            .shutdown_streams_by_addr(grpc_addr);
        if closed > 0 {
            tracing::info!(
                "[STATE] shutdown SSE streams on container destruction: grpc_addr={}, closed={}",
                grpc_addr,
                closed
            );
        }
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

    /// 会话创建 durable 直写（PG：内存 + 事务提交，返回即落库；降级不失败）。
    /// chat 完成点调用——"session_id 返回给前端 = 任何副本可服务"的契约。
    pub async fn insert_project_with_session_durable(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: &str,
    ) -> anyhow::Result<()> {
        self.projects
            .insert_project_with_session_durable(project_id, info, session_id)
            .await
    }

    /// 按 session_id 读（PG 模式 miss 回源直查一次 + hydrate；Memory 仅内存）
    pub async fn get_by_session_with_fetch(
        &self,
        session_id: &str,
    ) -> Option<Arc<ProjectAndContainerInfo>> {
        self.projects.get_by_session_with_fetch(session_id).await
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
    ///
    /// 存储契约（ProjectStore）只保留多 session 语义；本委托直接转发
    /// `add_session_to_project`（adapter 侧废弃同名方法也是该语义）。
    #[inline]
    #[deprecated(
        since = "0.0.0",
        note = "非原子，请用 `add_session_to_project` 走多 session 路径"
    )]
    pub fn update_session(&self, project_id: &str, session_id: &str) {
        let _ = self.projects.add_session_to_project(project_id, session_id);
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
        .route("/computer/cache/clean", post(handler::computer_cache_clean))
        .with_state(state.clone());

    // Pingora 代理 API 路由（用于文档和状态查询）
    let proxy_api_routes = Router::new()
        .route("/proxy/status", get(handler::proxy_status))
        .route("/proxy/stats", get(handler::proxy_stats))
        .route("/proxy/config", get(handler::proxy_config))
        // userApp 开发域终端/桌面代理的 307 文档接口（实际流量走 Pingora 8088；
        // 此处提供 Swagger 文档 + 可直接调用的重定向语义，对齐 devapp 先例）
        .route(
            "/userapp/ttyd/{app_id}/{*path}",
            get(handler::proxy_to_userapp_ttyd),
        )
        .route(
            "/userapp/ttyd/{app_id}",
            get(handler::proxy_to_userapp_ttyd_redirect_root),
        )
        .route(
            "/userapp/vnc/{app_id}/{*path}",
            get(handler::proxy_to_userapp_vnc),
        )
        .route(
            "/userapp/vnc/{app_id}",
            get(handler::proxy_to_userapp_vnc_redirect_root),
        )
        .route(
            "/userapp/audio/{app_id}/{*path}",
            get(handler::proxy_to_userapp_audio),
        )
        .route(
            "/userapp/audio/{app_id}",
            get(handler::proxy_to_userapp_audio_redirect_root),
        )
        .route(
            "/userapp/ime/{app_id}/{*path}",
            get(handler::proxy_to_userapp_ime),
        )
        .route(
            "/userapp/ime/{app_id}",
            get(handler::proxy_to_userapp_ime_redirect_root),
        )
        .route("/userapp/routes", get(handler::userapp_proxy_routes_doc))
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
        // 共享客户端 (连接超时 + 连接池复用; SSE 流不能设总超时, 见 http_client 模块)
        http_client: crate::http_client::shared_client().clone(),
    });
    let app_manager_routes = app_manager::routes::app_manager_routes()
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024)) // 1GiB（upload 压缩包，覆盖全局 50MB）
        .with_state(app_manager_state);

    // UserApp 自动化构建发布(rcoder 侧编排):publish/build + task 查询/SSE/cancel
    let userapp_publish_routes =
        crate::userapp_publish::handler::routes().with_state(state.clone());

    // userApp 文件域转发层: /api/userapp/{*rest} 通配透传 + create-workspace 显式入口
    let userapp_forward_routes = crate::userapp_forward::routes().with_state(state.clone());

    let mut router = Router::new()
        .merge(health_routes)
        .merge(api_routes)
        .merge(computer_routes)
        .merge(devcomputer_routes)
        .merge(proxy_api_routes)
        .merge(agent_mgmt_routes)
        .merge(app_manager_routes)
        .merge(userapp_publish_routes)
        .merge(userapp_forward_routes);

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
        .merge(crate::router_docs::create_swagger_ui())
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024)) // 50MB body 大小限制
        // HTTP 请求日志（target: tower_http → rcoder.log）+ W3C traceparent 提取
        // （入站 trace 贯通：e2e 注入 traceparent 时请求 span 继承远端 trace）
        .layer(
            tower_http::trace::TraceLayer::new_for_http().make_span_with(
                |req: &Request<axum::body::Body>| {
                    rcoder_telemetry::make_span_with_trace_parent(req)
                },
            ),
        )
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
        .merge(create_internal_routes(state.clone()))
        // file-server 基础路由（TS 移植版老路径：/api/project、/api/computer、/api/git、
        // /api/build、/api/page；排除 /api/userapp——由 rcoder 转发层接管）。
        // 与 TS 行为一致不设 API key → merge 在 api-key layer 之后（同 internal 先例）；
        // 构造失败不阻断主服务启动（warn 可见，缺路由面可诊断）。
        // computer 域拦截层：header X-Service-Type=userapp 的请求短路转发到该 app
        // 开发容器（反向代理转来的 TS 老路径，body 零解析）。
        .merge(match crate::file_server_embed::merged_router() {
            Ok(fs_router) => fs_router.layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::userapp_forward::computer_intercept,
            )),
            Err(e) => {
                tracing::warn!("file-server routes not mounted on main service: {e}");
                Router::new()
            }
        })
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
