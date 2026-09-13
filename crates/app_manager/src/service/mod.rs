//! Application lifecycle coordinator backed by the userApp lifecycle store.
//! SQLite (single-instance Docker) and PostgreSQL (Kubernetes) share admission,
//! ownership, generation and operation contracts. Runtime state remains the
//! authority for physical resources; short SQL transactions precede mutations.
//!
//! K8s 模式 `create_deployment` 创建 ConfigMap/Secret/ClusterIP Service/Deployment；
//! HTTP 入口按 `http_expose`：Pingora（默认，两后端统一，本服务注册 Pingora backend
//! `/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}` → 后端 host：Docker container_ip / K8s ClusterIP FQDN）或 Gateway
//! （可选，K8s 建 HTTPRoute `/apps/{id}`）。TCP 初期不对外。Docker 模式建容器入主网络。

use std::sync::Arc;
mod control;
mod operation_lock;
pub(crate) use control::OwnedOperation;
pub(crate) use operation_lock::AppOperationGuard;

use dashmap::DashMap;
use tracing::info;

use container_runtime_api::{DeploymentStatus, HttpExpose, UserAppRuntime};
use rcoder_proxy::PingoraProxyService;

use crate::AppActivityRegistry;
use crate::runtime::metadata::AppMetadataStore;

use super::config::{AppAccessMode, AppManagerConfig};
use super::models::*;
use super::utils::*;

/// 应用管理服务（Docker / K8s 统一）
pub struct AppService {
    pub(crate) config: AppManagerConfig,
    /// ISP 收紧 (阶段3): app_manager 只需 workspace (B) + Userapp Deployment (C) 能力,
    /// 不依赖 agent 容器生命周期 (A) —— 类型声明即编译期约束 (调用 agent 方法会编译错).
    pub(crate) runtime: Arc<dyn UserAppRuntime>,
    /// Userapp 活动状态注册表(闲置回收/流量唤醒的共享状态:last_accessed/stopped/waking)
    pub(crate) activity: Arc<AppActivityRegistry>,
    /// Pingora 代理（Docker 模式用于注册 HTTP backend；K8s 模式通常为 None）
    pub(crate) pingora: Option<Arc<PingoraProxyService>>,
    /// 路径解析器缓存（单例；Docker 模式将 rcoder 容器内路径解析为宿主机路径）
    /// Docker 模式 Pingora backend 端口登记（app_id → 注册的 HTTP 端口列表）
    ///
    /// 这是**操作副作用的临时缓存**（非业务元数据）：delete 时需要知道曾注册过哪些端口
    /// 才能清理 Pingora backend。rcoder 重启后丢失可接受（Docker 模式定位为开发环境）。
    pub(crate) pingora_ports: DashMap<String, Vec<u16>>,
    /// 同一 rcoder 进程内按 app 串行化 release 操作。PVC 文件锁继续负责跨进程互斥；
    /// 先等异步锁可避免同 app 的并发请求长期占用 Tokio blocking 线程等待 flock。
    pub(crate) release_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Persistent application identity, lifecycle and metadata; never an
    /// in-memory authority or a best-effort fallback after a storage failure.
    pub(crate) metadata: AppMetadataStore,
    /// Userapp 开发资源回收回调（宿主注入；purge 时回收 UserappBuilder 开发容器
    /// 与 per-app PVC——app_manager 的 runtime 视图无 agent 能力，经契约委托宿主）。
    pub(crate) dev_cleanup: std::sync::RwLock<Option<Arc<dyn shared_types::UserappDevCleanup>>>,
    /// Userapp 开发容器定位回调（宿主注入；文件/存储接口 `app_stage=dev` 分支经此
    /// ensure/定位 UserappBuilder 的 file-server——同 dev_cleanup 的委托根因）。
    pub(crate) builder_recovery:
        std::sync::RwLock<Option<Arc<dyn shared_types::UserAppBuilderRecovery>>>,
    pub(crate) dev_locator: std::sync::RwLock<Option<Arc<dyn shared_types::UserappDevLocator>>>,
    /// Deployment 列表查询缓存（TTL + 写路径失效 + single-flight）。防查询面
    /// 轮询频繁穿透到 Docker daemon/K8s apiserver——Docker daemon 高负载下
    /// API 可能无响应，穿透查询会挂死调用方（实战踩过：编译镜像期间 daemon
    /// 排队，容器操作全部卡住）。tokio Mutex 天然 single-flight：缓存过期时
    /// 并发请求只有一个穿透，其余等锁后直接命中新缓存（防击穿）。
    pub(crate) deploy_list_cache: tokio::sync::Mutex<Option<DeployListCacheEntry>>,
}

/// deploy_list_cache 的条目：写入时刻 + 列表快照。
pub(crate) struct DeployListCacheEntry {
    pub fetched_at: tokio::time::Instant,
    pub items: Vec<DeploymentStatus>,
}

impl AppService {
    /// Attach after constructing the owning Arc; the registry must not own us.
    pub fn attach_activity_coordinator(self: &Arc<Self>) -> AppResult<()> {
        self.activity.set_coordinator(Arc::downgrade(self))
    }

    /// 创建新的应用管理服务
    pub async fn new(
        config: AppManagerConfig,
        runtime: Arc<dyn UserAppRuntime>,
        activity: Arc<AppActivityRegistry>,
        pingora: Option<Arc<PingoraProxyService>>,
        store: Arc<dyn shared_types::UserAppLifecycleStore>,
    ) -> AppResult<Self> {
        if config.access_mode == AppAccessMode::Docker
            && config.operation_lock_root != shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT
        {
            return Err(AppOperationError::Validation(
                "Docker application operation lock root must match the shared runtime data root"
                    .into(),
            ));
        }
        // K8s 模式：启动时校验前置条件（RBAC 等）。失败直接返回，
        // 避免 rcoder 显示健康、直到首次创建 app 才暴露 403。
        if config.access_mode == AppAccessMode::Kubernetes {
            runtime
                .validate_app_prerequisites()
                .await
                .map_err(|error| {
                    map_runtime_error("[APP] K8s prerequisites validation failed", error)
                })?;
            info!("[APP] K8s prerequisites validated (RBAC/apps/deployments accessible)");
        }

        // 无效组合必须 Fail Fast：Docker 无 HTTPRoute/gateway 概念。
        if config.access_mode == AppAccessMode::Docker && config.http_expose == HttpExpose::Gateway
        {
            return Err(AppOperationError::Validation(
                "invalid app configuration: access_mode=docker requires http_expose=pingora".into(),
            ));
        }

        let svc = Self {
            config,
            runtime,
            activity,
            pingora,
            pingora_ports: DashMap::new(),
            deploy_list_cache: tokio::sync::Mutex::new(None),
            release_locks: DashMap::new(),
            metadata: AppMetadataStore::new(store),
            dev_cleanup: std::sync::RwLock::new(None),
            dev_locator: std::sync::RwLock::new(None),
            builder_recovery: std::sync::RwLock::new(None),
        };
        // K8s Pingora 模式：启动时从集群重建 Pingora backends——修复 pingora_ports 内存态
        // 丢失导致的重启 silent 404（list_deployments 的 expose_type 已由 Deployment annotation
        // 准确还原）。重建失败时不能对外声称就绪。
        if svc.config.access_mode == AppAccessMode::Kubernetes
            && svc.config.http_expose == HttpExpose::Pingora
        {
            svc.rebuild_pingora_backends().await?;
        }
        // 重建活动状态内存态(rcoder 重启后 last_accessed/stopped 丢失):
        //  - replicas==0 → mark_stopped(支持流量唤醒识别)
        //  - replicas>0  → seed_accessed=now(给 Running app 完整 grace 周期,避免重启后立刻被回收)
        // 失败时 stopped app 无法被流量唤醒，必须阻止就绪。
        svc.rebuild_stopped_apps().await?;
        Ok(svc)
    }

    /// 获取该 app 的进程级发布锁（create/update/start-deploy/delete 串行化）。
    /// 原 release_flow/releases.rs 遗产——卷上 releases 编排删除后锁本身仍服务
    /// 生命周期互斥，迁入 service.rs（`release_locks` 字段所在）。
    pub(crate) async fn acquire_process_release_lock(
        &self,
        app_id: &str,
    ) -> AppResult<AppOperationGuard> {
        let lock = match self.release_locks.entry(app_id.to_owned()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                entry.insert(lock.clone());
                lock
            }
        };
        self.operation_guard(app_id, lock.lock_owned().await, true)
            .await
    }

    pub(crate) async fn try_acquire_process_release_lock(
        &self,
        app_id: &str,
    ) -> AppResult<AppOperationGuard> {
        let lock = self
            .release_locks
            .entry(app_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let process = lock.try_lock_owned().map_err(|_| {
            AppOperationError::Conflict("application operation is in progress".into())
        })?;
        self.operation_guard(app_id, process, false).await
    }

    /// 锁条目无人持有（strong_count==1，仅 map 自身）时移除，防 DashMap 无界增长。
    pub(crate) fn remove_unused_process_release_lock(&self, app_id: &str) {
        if let dashmap::mapref::entry::Entry::Occupied(entry) =
            self.release_locks.entry(app_id.to_owned())
            && Arc::strong_count(entry.get()) == 1
        {
            entry.remove();
        }
    }
}

// list/query/get/update/delete 编排实现拆至 lifecycle/{query,update}.rs（extension-impl）。
#[async_trait::async_trait]
impl super::AppServiceTrait for AppService {
    async fn retry_control_operation(
        &self,
        app_id: &str,
        operation_id: &str,
        request: shared_types::UserAppRetryRequest,
    ) -> AppResult<shared_types::UserAppOperationView> {
        self.retry_control_operation(app_id, operation_id, request)
            .await
    }
    async fn resume_pending_control(
        &self,
        operation: &shared_types::UserAppOperationRecord,
    ) -> AppResult<bool> {
        self.resume_pending_control(operation).await
    }

    async fn get_lifecycle(
        &self,
        app_id: &str,
        user_id: &str,
    ) -> AppResult<shared_types::UserAppLifecycleRecord> {
        self.get_lifecycle(app_id, user_id).await
    }
    async fn get_control_operation(
        &self,
        app_id: &str,
        user_id: &str,
        operation_id: Option<&str>,
    ) -> AppResult<Option<shared_types::UserAppOperationView>> {
        self.get_control_operation(app_id, user_id, operation_id)
            .await
    }
    async fn get_control_operation_by_request(
        &self,
        app_id: &str,
        user_id: &str,
        request_id: &str,
    ) -> AppResult<Option<shared_types::UserAppOperationView>> {
        self.get_control_operation_by_request(app_id, user_id, request_id)
            .await
    }
    async fn recreate_identity(
        &self,
        app_id: &str,
        request: shared_types::UserAppRecreateRequest,
    ) -> AppResult<shared_types::UserAppLifecycleRecord> {
        self.recreate_identity(app_id, request).await
    }

    async fn record_dev_registration(&self, app_id: &str, user_id: &str) -> AppResult<()> {
        self.metadata.store.ensure_identity(app_id, user_id).await?;
        Ok(())
    }

    async fn create_app(&self, request: CreateAppRequest) -> AppResult<AppInfo> {
        self.create_app(request).await
    }

    async fn query_apps(
        &self,
        request: QueryAppsRequest,
    ) -> AppResult<PaginatedResponse<AppRuntimeInfo>> {
        self.query_apps(request).await
    }

    async fn list_app_runtimes(&self, user_id: &str) -> AppResult<Vec<AppRuntimeInfo>> {
        self.list_app_runtimes(user_id).await
    }

    async fn list_all_app_runtimes(&self) -> AppResult<Vec<AppRuntimeInfo>> {
        self.list_all_app_runtimes().await
    }

    async fn get_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.get_app(app_id).await
    }

    async fn update_app(
        &self,
        app_id: &str,
        request: UpdateAppRequest,
    ) -> AppResult<AppRuntimeInfo> {
        self.update_app(app_id, request).await
    }

    async fn delete_app(
        &self,
        app_id: &str,
        purge: bool,
        expected_resource_version: Option<&str>,
    ) -> AppResult<()> {
        self.delete_app(app_id, purge, expected_resource_version)
            .await
    }

    async fn delete_app_controlled(
        &self,
        app_id: &str,
        request: DeleteAppRequest,
    ) -> AppResult<()> {
        self.delete_app_controlled(app_id, request).await
    }

    async fn purge_app(&self, app_id: &str) -> AppResult<()> {
        self.purge_app(app_id).await
    }

    async fn purge_app_controlled(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
    ) -> AppResult<()> {
        self.purge_app_controlled(app_id, request).await
    }

    async fn get_app_storage(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<StorageInfo> {
        self.get_app_storage(app_stage, app_id).await
    }

    async fn clear_app_storage_controlled(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        request: ClearStorageRequest,
    ) -> AppResult<String> {
        self.clear_app_storage_controlled(app_stage, app_id, request)
            .await
    }
    async fn clear_app_storage(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
    ) -> AppResult<()> {
        self.clear_app_storage(app_stage, app_id, user_id).await
    }

    async fn destroy_app_storage_controlled(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        request: DestroyStorageRequest,
    ) -> AppResult<String> {
        self.destroy_app_storage_controlled(app_stage, app_id, request)
            .await
    }
    async fn destroy_app_storage(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
        confirm: &str,
    ) -> AppResult<()> {
        self.destroy_app_storage(app_stage, app_id, user_id, confirm)
            .await
    }

    async fn align_db_credentials(
        &self,
        app_id: &str,
        request: shared_types::AlignCredentialsRequest,
    ) -> AppResult<shared_types::AlignCredentialsOutcome> {
        self.align_db_credentials(app_id, request).await
    }

    async fn execute_database_sql(&self, app_id: &str) -> AppResult<DatabaseSqlReport> {
        self.execute_database_sql(app_id).await
    }

    async fn get_app_owner(&self, app_id: &str) -> AppResult<Option<String>> {
        Ok(self.metadata.lookup(app_id).await?.and_then(|r| r.user_id))
    }

    async fn query_storage(
        &self,
        app_stage: shared_types::UserappStage,
        request: QueryStorageRequest,
    ) -> AppResult<PaginatedResponse<StorageInfo>> {
        self.query_storage(app_stage, request).await
    }

    async fn start_app_enhanced(
        &self,
        app_id: &str,
        request: StartAppRequest,
    ) -> AppResult<StartAppResult> {
        self.start_app_enhanced(app_id, request).await
    }

    async fn restart_app_enhanced(
        &self,
        app_id: &str,
        request: StartAppRequest,
    ) -> AppResult<StartAppResult> {
        self.restart_app_enhanced(app_id, request).await
    }

    async fn start_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.start_app(app_id).await
    }

    async fn start_app_controlled(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
    ) -> AppResult<AppRuntimeInfo> {
        self.start_app_controlled(app_id, request).await
    }

    async fn stop_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.stop_app(app_id).await
    }
    async fn stop_app_controlled(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
    ) -> AppResult<AppRuntimeInfo> {
        self.stop_app_controlled(app_id, request).await
    }

    async fn recycle_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.recycle_app(app_id).await
    }

    async fn restart_app_controlled(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
    ) -> AppResult<AppRuntimeInfo> {
        self.restart_app_controlled(app_id, request).await
    }

    async fn restart_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.restart_app(app_id).await
    }

    async fn set_recycle_policy(
        &self,
        app_id: &str,
        request: RecyclePolicyRequest,
    ) -> AppResult<AppRuntimeInfo> {
        self.set_recycle_policy(app_id, request).await
    }

    async fn get_app_stats(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<ResourceStats> {
        self.get_app_stats(app_stage, app_id).await
    }

    async fn get_app_health(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
    ) -> AppResult<HealthInfo> {
        self.get_app_health(app_stage, app_id).await
    }

    async fn log_api_base(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
    ) -> AppResult<String> {
        self.log_api_base(app_stage, app_id, user_id).await
    }

    async fn get_app_events(
        &self,
        app_id: &str,
    ) -> AppResult<Vec<container_runtime_api::AppEventInfo>> {
        self.get_app_events(app_id).await
    }

    async fn upload_file(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
        file_data: Vec<u8>,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult> {
        self.upload_file(app_stage, app_id, user_id, file_data, target, flatten)
            .await
    }

    async fn upload_from_url(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
        url: &str,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult> {
        self.upload_from_url(app_stage, app_id, user_id, url, target, flatten)
            .await
    }

    async fn list_files(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
        subpath: Option<&str>,
    ) -> AppResult<Vec<FileInfo>> {
        self.list_files(app_stage, app_id, user_id, subpath).await
    }

    async fn delete_file(
        &self,
        app_stage: shared_types::UserappStage,
        app_id: &str,
        user_id: &str,
        file_path: &str,
    ) -> AppResult<()> {
        self.delete_file(app_stage, app_id, user_id, file_path)
            .await
    }
}

#[cfg(test)]
mod tests;
