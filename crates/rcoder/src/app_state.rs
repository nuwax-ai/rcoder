//! 应用状态与项目/会话注册表（从 router.rs 拆出——状态管理自成一档）。
//!
//! [`AppState`]：全局共享状态（配置/容器查找/项目注册表/SSE 流管理/活动注册表），
//! 含项目增删与 session 归属/活跃度维护方法；路由组装仍在 [`crate::router`]。
//!
//! lib+bin 双树：部分方法仅单树消费，与 router.rs 同款抑制 lib 维度 dead_code 误报。
#![allow(dead_code)]

use std::sync::Arc;

use arc_swap::ArcSwap;
use container_runtime_api::ContainerRuntime;
use dashmap::DashMap;
use serde::Serialize;
use shared_types::ProjectAndContainerInfo;
use tokio::sync::broadcast;

use crate::{
    config::{ApiKeyAuthConfig, AppConfig},
    storage::ProjectStoreBackend,
};
use agent_provisioning::AgentDownloadManager;

// 存储契约 trait 引入作用域：AppState.projects 为枚举 ProjectStoreBackend，
// 其上的 get/insert/... 方法均经 shared_types::ProjectStore 解析。
use shared_types::ProjectStore as _;

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
    ) -> anyhow::Result<Arc<Self>> {
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

        // 初始化应用管理服务（Docker / K8s 统一构造，运行时由 access_mode 决定行为）。
        // 保留具体类型 Arc：dev_locator 注入需要在其上调用 inherent setter
        // （发生在下方 Self Arc 包装之后——locator 以 Weak 回指 state）。
        let app_service_arc: Arc<app_manager::service::AppService> = Arc::new(
            app_manager::service::AppService::new(
                config.app_manager.clone(),
                runtime.clone(),
                activity.clone(),
                pingora.clone(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("failed to initialize app service: {}", e))?,
        );

        // UserApp 开发资源回收回调（app purge 时回收 UserAppBuilder 开发容器 +
        // per-app PVC；app_manager 的 runtime 视图无 agent 能力，经契约委托本进程）
        app_service_arc.set_dev_cleanup(Arc::new(
            crate::userapp_builder::UserappDevResourcesCleanup::new(
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
                        app_service_arc.set_metadata_persistence(metadata_persistence);
                        app_service_arc.apply_metadata_loaded(rows);
                    }
                    Err(e) => {
                        anyhow::bail!("[STORAGE_PG] userapp_metadata load failed: {e:#}");
                    }
                }
            }
        }
        let app_service: Arc<dyn app_manager::AppServiceTrait> = app_service_arc.clone();

        let state = Arc::new(Self {
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
        });

        // userApp 文件/存储接口 env=dev 分支的开发容器定位回调（幂等 ensure +
        // 探活自愈 + file-server 地址解析）。Weak 挂接防
        // AppState → app_service → dev_locator → AppState 引用环。
        app_service_arc.set_dev_locator(Arc::new(crate::userapp_builder::UserappDevLocator::new(
            Arc::downgrade(&state),
        )));

        Ok(state)
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

    /// 删除 project——durable 变体（stop/清理路径，消队列倒挂窗口）。
    pub async fn remove_project_durable(
        &self,
        project_id: &str,
    ) -> Option<Arc<ProjectAndContainerInfo>> {
        let removed = self.projects.remove_durable(project_id).await;
        // session 终结：归还 served_sessions 首连资格条目（防无界累积；调用方
        // 已先行 shutdown_sse_streams_for_project 关流，此处只清资格登记）
        if let Some(info) = &removed {
            for sid in info.sessions() {
                self.session_stream_registry
                    .release_first_client_claim(&sid);
            }
        }
        removed
    }

    /// 容器级删除（容器销毁路径，连带全部 project 记录）——durable 变体。
    ///
    /// 与 [`Self::remove_project_durable`] 同语义：删除前按 container_id 归还
    /// 各 project 的 served_sessions 资格（直连 backend 会漏——被 cancel 的
    /// 转发 task 不走 turn 终态 release，条目泄漏到进程重启）。
    /// 返回 (容器是否删除, 关联 project 数)。
    pub async fn delete_container_with_projects_durable(
        &self,
        container_id: &str,
    ) -> (bool, usize) {
        // 删除前枚举该容器全部 project 的 sessions 归还资格（backend 删除后
        // 记录即消失，无法再据此枚举）
        let sids: Vec<String> = self
            .projects
            .iter()
            .into_iter()
            .filter(|(_, info)| {
                info.container_info()
                    .is_some_and(|c| c.container_id == container_id)
            })
            .flat_map(|(_, info)| info.sessions().into_iter())
            .collect();
        let result = self
            .projects
            .delete_container_with_projects_durable(container_id)
            .await;
        for sid in sids {
            self.session_stream_registry
                .release_first_client_claim(&sid);
        }
        result
    }

    pub fn remove_project(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        let removed = self.projects.remove(project_id);
        // session 终结：归还 served_sessions 首连资格条目（防无界累积；调用方
        // 已先行 shutdown_sse_streams_for_project 关流，此处只清资格登记）
        if let Some(info) = &removed {
            for sid in info.sessions() {
                self.session_stream_registry
                    .release_first_client_claim(&sid);
            }
        }
        removed
    }

    /// 关闭某 project 关联的所有 SSE 共享流（容器销毁/项目删除前调用）。
    ///
    /// 必须在 [`Self::remove_project`] 之前调用——remove_project 会清空 project 的 sessions 集合，
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

    /// 物理销毁容器后关闭其 SSE 共享流并清理 gRPC 连接（post-destroy；按 addr
    /// 关闭幂等，不依赖 project/session 记录）。地址走 `build_grpc_addr`
    /// （K8s Service FQDN / Docker 容器 IP，与连接建立同源）。K8s 恒清；
    /// Docker 仅在有 IP 时清。
    pub async fn teardown_container_connections(&self, container_name: &str, container_ip: &str) {
        if shared_types::is_kubernetes_runtime() || !container_ip.is_empty() {
            let addr = shared_types::build_grpc_addr(
                container_name,
                container_ip,
                &self.config.app_manager.namespace,
                &self.cluster_domain,
            );
            self.shutdown_sse_streams_by_addr(&addr);
            self.grpc_pool.remove(&addr).await;
        }
    }

    /// 按 grpc_addr 关闭关联的所有 SSE 共享流。
    ///
    /// 适用于记录可能已被清空的销毁路径（reaper/restart/ensure/destroyer）：这些路径中
    /// project/session 记录可能在关闭前已被移除，无法再走 [`Self::shutdown_sse_streams_for_project`]，
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

    /// 追加 session 的 durable 变体（/chat 响应后映射补录）——跨副本可见性
    /// 契约：返回 Ok(true) 即主库已提交（PG 内部超时降级 write-behind）。
    /// Memory 模式等价普通内存写。返回 Ok(false) = project 不存在。
    pub async fn add_session_durable(
        &self,
        project_id: &str,
        session_id: &str,
    ) -> anyhow::Result<bool> {
        self.projects
            .add_session_durable(project_id, session_id)
            .await
    }

    /// 清除会话信息（清所有 session，agent stop 场景）——durable 变体：
    /// stop 路径调用（与插入类 durable 同事务语义，消队列倒挂窗口）。
    pub async fn clear_session_durable(&self, project_id: &str) {
        // 先取 sessions 快照：session 终结时同步归还 served_sessions 首连资格
        // 条目——turn 进行中客户端全部断开的 session 不会再触发流侧 release，
        // 不在此清理会按历史 session 数无界累积
        let sids: Vec<String> = self
            .get_project(project_id)
            .map(|info| info.sessions().into_iter().collect())
            .unwrap_or_default();
        self.projects.clear_session_durable(project_id).await;
        for sid in sids {
            self.session_stream_registry
                .release_first_client_claim(&sid);
        }
    }

    /// 清除会话信息（同步版：非 stop 路径/测试用；stop 链路走 durable 版）
    pub fn clear_session(&self, project_id: &str) {
        let sids: Vec<String> = self
            .get_project(project_id)
            .map(|info| info.sessions().into_iter().collect())
            .unwrap_or_default();
        self.projects.clear_session(project_id);
        for sid in sids {
            self.session_stream_registry
                .release_first_client_claim(&sid);
        }
    }

    /// 清除单个 session（保留 project 的其他 session）——durable 变体。
    pub async fn clear_session_one_durable(&self, project_id: &str, session_id: &str) -> bool {
        let removed = self
            .projects
            .clear_session_one_durable(project_id, session_id)
            .await;
        if removed {
            self.session_stream_registry
                .release_first_client_claim(session_id);
        }
        removed
    }

    /// 清除单个 session（同步版）
    pub fn clear_session_one(&self, project_id: &str, session_id: &str) -> bool {
        let removed = self.projects.clear_session_one(project_id, session_id);
        if removed {
            self.session_stream_registry
                .release_first_client_claim(session_id);
        }
        removed
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
