//! 应用管理服务层（统一 Docker / K8s 后端，无状态）
//!
//! rcoder 是无状态的应用 pod 引擎：
//! - 写操作（create/start/stop/restart/delete）转调 [`ContainerRuntime`] 的 Deployment 能力；
//! - 读操作（get/query/list）实时查集群，返回 [`AppRuntimeInfo`]；
//! - 业务元数据（name/image/command/env 等）由调用方（Java）持久化，rcoder 不存。
//!
//! K8s 模式 `create_deployment` 创建 ConfigMap/Secret/ClusterIP Service/Deployment；
//! HTTP 入口按 `http_expose`：Pingora（默认，两后端统一，本服务注册 Pingora backend
//! `/proxy/userapp/prod/{user_id}/{app_id}` → 后端 host：Docker container_ip / K8s ClusterIP FQDN）或 Gateway
//! （可选，K8s 建 HTTPRoute `/apps/{id}`）。TCP 初期不对外。Docker 模式建容器入主网络。

use std::sync::Arc;

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
    /// ISP 收紧 (阶段3): app_manager 只需 workspace (B) + UserApp Deployment (C) 能力,
    /// 不依赖 agent 容器生命周期 (A) —— 类型声明即编译期约束 (调用 agent 方法会编译错).
    pub(crate) runtime: Arc<dyn UserAppRuntime>,
    /// UserApp 活动状态注册表(闲置回收/流量唤醒的共享状态:last_accessed/stopped/waking)
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
    /// 应用业务元数据（name/租户/业务创建时间;集群不持有）。PG 模式 query 的
    /// name/created_at 过滤数据源；纯内存模式恒空（过滤忽略+warn）。
    pub(crate) metadata: AppMetadataStore,
    /// UserApp 开发资源回收回调（宿主注入；purge 时回收 UserAppBuilder 开发容器
    /// 与 per-app PVC——app_manager 的 runtime 视图无 agent 能力，经契约委托宿主）。
    pub(crate) dev_cleanup: std::sync::RwLock<Option<Arc<dyn shared_types::UserappDevCleanup>>>,
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
    /// 创建新的应用管理服务
    pub async fn new(
        config: AppManagerConfig,
        runtime: Arc<dyn UserAppRuntime>,
        activity: Arc<AppActivityRegistry>,
        pingora: Option<Arc<PingoraProxyService>>,
    ) -> AppResult<Self> {
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
            metadata: AppMetadataStore::default(),
            dev_cleanup: std::sync::RwLock::new(None),
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
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = match self.release_locks.entry(app_id.to_owned()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                entry.insert(lock.clone());
                lock
            }
        };
        lock.lock_owned().await
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
    async fn record_dev_registration(&self, app_id: &str, user_id: &str) -> AppResult<()> {
        // 开发注册：owner user_id 权威覆盖；已部署 app 的业务字段（name/tenant/space
        // 由 create_app 落库）合并保留——record 是整行 upsert，全传 None 会把
        // 已部署应用的 name/租户列 NULL 掉（query-by-name 过滤随之失效）
        let existing = self.metadata.lookup(app_id);
        let (name, tenant, space) = match &existing {
            Some(row) => (
                row.name.clone(),
                row.tenant_id.clone(),
                row.space_id.clone(),
            ),
            None => (None, None, None),
        };
        self.metadata
            .record(app_id, name, Some(user_id.to_string()), tenant, space)
            .await;
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

    async fn list_app_runtimes(&self) -> AppResult<Vec<AppRuntimeInfo>> {
        self.list_app_runtimes().await
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

    async fn get_app_storage(&self, app_id: &str) -> AppResult<StorageInfo> {
        self.get_app_storage(app_id).await
    }

    async fn clear_app_storage(&self, app_id: &str) -> AppResult<()> {
        self.clear_app_storage(app_id).await
    }

    async fn destroy_app_storage(&self, app_id: &str, confirm: &str) -> AppResult<()> {
        self.destroy_app_storage(app_id, confirm).await
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

    async fn get_app_owner(&self, app_id: &str) -> Option<String> {
        self.metadata.lookup(app_id).and_then(|r| r.user_id)
    }

    async fn query_storage(
        &self,
        request: QueryStorageRequest,
    ) -> AppResult<PaginatedResponse<StorageInfo>> {
        self.query_storage(request).await
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

    async fn stop_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.stop_app(app_id).await
    }

    async fn recycle_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.recycle_app(app_id).await
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

    async fn get_app_stats(&self, app_id: &str) -> AppResult<ResourceStats> {
        self.get_app_stats(app_id).await
    }

    async fn get_app_events(
        &self,
        app_id: &str,
    ) -> AppResult<Vec<container_runtime_api::AppEventInfo>> {
        self.get_app_events(app_id).await
    }

    async fn upload_file(
        &self,
        app_id: &str,
        file_data: Vec<u8>,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult> {
        self.upload_file(app_id, file_data, target, flatten).await
    }

    async fn upload_from_url(
        &self,
        app_id: &str,
        url: &str,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult> {
        self.upload_from_url(app_id, url, target, flatten).await
    }

    async fn list_files(&self, app_id: &str, subpath: Option<&str>) -> AppResult<Vec<FileInfo>> {
        self.list_files(app_id, subpath).await
    }

    async fn delete_file(&self, app_id: &str, file_path: &str) -> AppResult<()> {
        self.delete_file(app_id, file_path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    use crate::models::ResourceLimits;
    use crate::test_support::{MockRuntime, release_lock, test_service};
    use container_runtime_api::StorageResizeOutcome;

    pub(crate) fn create_request(app_id: &str) -> CreateAppRequest {
        CreateAppRequest {
            app_id: Some(app_id.to_owned()),
            name: "r2-app".into(),
            user_id: "u-test".to_string(),
            image: Some("registry.example/app-runtime:test".into()),
            command: None,
            env: None,
            secrets: None,
            resources: None,
            ports: None,
            health_check: None,
            tenant_id: None,
            space_id: None,
            recycle_enabled: None,
            idle_timeout_seconds: None,
        }
    }

    /// R2：create_app_runtime 失败——断言 delete_deployment 兜底被调用、原始错误原样返回（不被清理覆盖）。
    #[tokio::test]
    pub(crate) async fn create_app_runtime_failure_triggers_best_effort_cleanup() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        runtime.create_fails.store(true, Ordering::SeqCst);
        let service = test_service(root.path(), runtime.clone());
        // build_container_params 需 code/release.lock.toml，预铺现场
        let app_dir = root.path().join("app-r2");
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");

        let error = service
            .create_app(create_request("app-r2"))
            .await
            .expect_err("create_app must fail");

        // 原始错误原样返回（create_deployment 失败的映射，未被清理逻辑覆盖）
        assert!(
            error.to_string().contains("create_deployment failed"),
            "original error must be preserved, got: {error}"
        );
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime.delete_calls.load(Ordering::SeqCst),
            1,
            "delete_deployment fallback must be called after create_app_runtime failure"
        );
    }

    /// R2 对照：清理自身失败也不改变原始错误（只 warn）。
    #[tokio::test]
    pub(crate) async fn create_app_cleanup_failure_keeps_original_error() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        runtime.create_fails.store(true, Ordering::SeqCst);
        runtime.delete_fails.store(true, Ordering::SeqCst);
        let service = test_service(root.path(), runtime.clone());
        let app_dir = root.path().join("app-r2b");
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");

        let error = service
            .create_app(create_request("app-r2b"))
            .await
            .expect_err("create_app must fail");

        assert!(
            error.to_string().contains("create_deployment failed"),
            "original error must not be masked by cleanup failure, got: {error}"
        );
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    }

    /// 回归（userapp_metadata）：update 不带 name（name 是"仅元数据"调用方常省略）
    /// 不得清空已存业务名——否则 query name 过滤对该 app 永久失效。带 name 则覆盖。
    #[tokio::test]
    pub(crate) async fn update_app_without_name_keeps_metadata_name() {
        use crate::models::UpdateAppRequest;

        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root.path(), runtime);
        // create_app 需要 code/release.lock.toml
        let app_dir = root.path().join("app-meta");
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");

        let mut create = create_request("app-meta");
        create.name = "alpha".into();
        service.create_app(create).await.expect("create app");
        assert_eq!(
            service.metadata.lookup("app-meta").and_then(|m| m.name),
            Some("alpha".into()),
            "create records name"
        );

        let update_no_name = UpdateAppRequest {
            name: None,
            image: Some("registry.example/app-runtime:v2".into()),
            env: None,
            secrets: None,
            resources: None,
            tenant_id: None,
            space_id: None,
            recycle_enabled: None,
            idle_timeout_seconds: None,
            expected_resource_version: None,
        };
        service
            .update_app("app-meta", update_no_name.clone())
            .await
            .expect("update without name");
        assert_eq!(
            service.metadata.lookup("app-meta").and_then(|m| m.name),
            Some("alpha".into()),
            "update without name must NOT clear recorded name"
        );

        let mut update_with_name = update_no_name;
        update_with_name.image = Some("registry.example/app-runtime:v3".into());
        update_with_name.name = Some("beta".into());
        service
            .update_app("app-meta", update_with_name)
            .await
            .expect("update with name");
        assert_eq!(
            service.metadata.lookup("app-meta").and_then(|m| m.name),
            Some("beta".into()),
            "explicit name overrides"
        );
    }

    /// update 与发布并发：发布锁被占 → 立即 409（不排队傻等 activate 的就绪窗口）。
    #[tokio::test]
    pub(crate) async fn update_app_conflicts_while_release_lock_held() {
        let root = tempfile::tempdir().expect("tempdir");
        let service = test_service(root.path(), Arc::new(MockRuntime::default()));
        let _publish_lock = service.acquire_process_release_lock("app-busy").await;

        let request = UpdateAppRequest {
            name: None,
            image: Some("registry.example/app-runtime:v2".into()),
            env: None,
            secrets: None,
            resources: None,
            tenant_id: None,
            space_id: None,
            recycle_enabled: None,
            idle_timeout_seconds: None,
            expected_resource_version: None,
        };
        let error = service
            .update_app("app-busy", request)
            .await
            .expect_err("update during publish must 409");
        assert!(
            matches!(error, AppOperationError::Conflict(_)),
            "got: {error}"
        );
    }

    /// update 前置：create 一个 running app（fetch_runtime_status 需要 Deployment
    /// 存在），返回 service 与 runtime 句柄（resize/patch 调用断言用）。
    async fn created_app_service(
        root: &std::path::Path,
        app_id: &str,
    ) -> (AppService, Arc<MockRuntime>) {
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root, runtime.clone());
        let app_dir = root.join(app_id);
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");
        service
            .create_app(create_request(app_id))
            .await
            .expect("create app");
        (service, runtime)
    }

    fn update_request_with_storage(storage: Option<&str>) -> UpdateAppRequest {
        UpdateAppRequest {
            name: None,
            image: Some("registry.example/app-runtime:v2".into()),
            env: None,
            secrets: None,
            resources: storage.map(|s| ResourceLimits {
                cpu: None,
                memory: None,
                storage: Some(s.to_string()),
                ephemeral_storage: None,
            }),
            tenant_id: None,
            space_id: None,
            recycle_enabled: None,
            idle_timeout_seconds: None,
            expected_resource_version: None,
        }
    }

    /// update 带 resources.storage → resize_app_storage 收到扩容目标，update 整体成功。
    #[tokio::test]
    pub(crate) async fn update_app_storage_resize_triggered() {
        let root = tempfile::tempdir().expect("tempdir");
        let (service, runtime) = created_app_service(root.path(), "app-resize").await;
        let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

        service
            .update_app("app-resize", update_request_with_storage(Some("200Gi")))
            .await
            .expect("update with storage");

        assert_eq!(
            runtime.resize_calls.get("app-resize").map(|c| c.clone()),
            Some(vec!["200Gi".to_string()]),
            "resize target forwarded"
        );
        assert_eq!(
            runtime.create_calls.load(Ordering::SeqCst),
            create_calls_before + 1,
            "patch_deployment still applied after successful resize"
        );
    }

    /// 缩容拒绝（ShrinkRejected）→ update 整体 400 Validation，且 patch 不再执行
    /// （resize 在 patch 之前——阻断顺序防"语义错误却滚动生效"）。
    #[tokio::test]
    pub(crate) async fn update_app_storage_shrink_rejected_blocks_update() {
        let root = tempfile::tempdir().expect("tempdir");
        let (service, runtime) = created_app_service(root.path(), "app-shrink").await;
        *runtime.resize_outcome.lock().expect("outcome lock") =
            Some(StorageResizeOutcome::ShrinkRejected {
                current: "200Gi".into(),
                requested: "50Gi".into(),
            });
        let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

        let error = service
            .update_app("app-shrink", update_request_with_storage(Some("50Gi")))
            .await
            .expect_err("shrink must be rejected");
        assert!(
            matches!(error, AppOperationError::Validation(_)),
            "got: {error}"
        );
        assert_eq!(
            runtime.create_calls.load(Ordering::SeqCst),
            create_calls_before,
            "patch_deployment must NOT run when resize rejected"
        );
    }

    /// resize 后端失败 → update 整体失败（storage 字段承诺生效，不静默降级）。
    #[tokio::test]
    pub(crate) async fn update_app_storage_resize_failure_blocks_update() {
        let root = tempfile::tempdir().expect("tempdir");
        let (service, runtime) = created_app_service(root.path(), "app-rfail").await;
        runtime.resize_fails.store(true, Ordering::SeqCst);
        let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

        let error = service
            .update_app("app-rfail", update_request_with_storage(Some("200Gi")))
            .await
            .expect_err("resize failure must block update");
        assert!(
            matches!(error, AppOperationError::Backend(_)),
            "got: {error}"
        );
        assert_eq!(
            runtime.create_calls.load(Ordering::SeqCst),
            create_calls_before,
            "patch_deployment must NOT run when resize failed"
        );
    }

    /// update 不带 resources.storage（None 或无 storage 字段）→ resize 不触发。
    #[tokio::test]
    pub(crate) async fn update_app_without_storage_skips_resize() {
        let root = tempfile::tempdir().expect("tempdir");
        let (service, runtime) = created_app_service(root.path(), "app-nosize").await;

        service
            .update_app("app-nosize", update_request_with_storage(None))
            .await
            .expect("update without storage");

        assert!(
            runtime.resize_calls.is_empty(),
            "resize must not be called without storage"
        );
    }

    /// query_apps 分页校验：page<1 / page_size∉[1,100] → 400（对齐 query_storage 与
    /// publish tasks 口径；此前静默 clamp，超大 page 在 debug 构建乘法溢出 panic）。
    #[tokio::test]
    pub(crate) async fn query_apps_rejects_invalid_pagination_and_sort() {
        let service = test_service(
            tempfile::tempdir().expect("tempdir").path(),
            Arc::new(MockRuntime::default()),
        );
        for (page, page_size) in [(0u32, 20u32), (1, 0), (1, 101)] {
            let request = QueryAppsRequest {
                page: Some(page),
                page_size: Some(page_size),
                ..QueryAppsRequest::default()
            };
            let error = service
                .query_apps(request)
                .await
                .expect_err("invalid pagination must 400");
            assert!(
                matches!(error, AppOperationError::Validation(_)),
                "page={page} page_size={page_size}: {error}"
            );
        }
        let request = QueryAppsRequest {
            sort_by: Some("bogus".into()),
            ..QueryAppsRequest::default()
        };
        let error = service
            .query_apps(request)
            .await
            .expect_err("invalid sort_by must 400");
        assert!(matches!(error, AppOperationError::Validation(_)));
    }

    /// 三档删除语义：delete(purge=true) 销毁存储但**保留**元数据行（误删找回）；
    /// 仅独立 storage/destroy 接口删行。
    #[tokio::test]
    pub(crate) async fn delete_app_purge_keeps_metadata_row_until_explicit_destroy() {
        use crate::test_support::InMemoryMetadataPersistence;
        use shared_types::AppMetadataPersistence as _;

        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let persistence = InMemoryMetadataPersistence::new(vec![]);
        let service = test_service(root.path(), runtime);
        service.set_metadata_persistence(persistence.clone());
        let app_dir = root.path().join("app-purge");
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");

        let mut create = create_request("app-purge");
        create.name = "keep-me".into();
        service.create_app(create).await.expect("create app");
        assert!(service.metadata.lookup("app-purge").is_some());

        service
            .delete_app("app-purge", true, None)
            .await
            .expect("purge delete");
        assert!(
            service.metadata.lookup("app-purge").is_some(),
            "purge must retain metadata row (three-tier contract)"
        );
        assert!(
            persistence
                .load_all()
                .await
                .expect("persisted")
                .iter()
                .any(|r| r.app_id == "app-purge"),
            "PG row retained after purge"
        );

        service
            .destroy_app_storage("app-purge", "app-purge")
            .await
            .expect("explicit destroy");
        assert!(
            service.metadata.lookup("app-purge").is_none(),
            "explicit storage destroy deletes metadata row"
        );
    }

    /// query_apps 的 name/created_at 过滤:纯内存模式（无 metadata 持久化）维持忽略
    /// （全量返回,旧行为）;注入持久化（PG 模式同构）后经内存 join 生效。
    #[tokio::test]
    pub(crate) async fn query_apps_name_filter_respects_metadata_mode() {
        use crate::test_support::InMemoryMetadataPersistence;
        use container_runtime_api::DeploymentStatus;
        use shared_types::{AppMetadataPersistence as _, AppMetadataRecord};

        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        for app_id in ["app-alpha", "app-beta"] {
            runtime.deployments.insert(
                app_id.into(),
                DeploymentStatus {
                    app_id: app_id.into(),
                    ..Default::default()
                },
            );
        }
        let service = test_service(root.path(), runtime.clone());

        let by_name = |name: &str| QueryAppsRequest {
            page: None,
            page_size: None,
            filters: Some(AppFilters {
                status: None,
                name: Some(name.into()),
                app_ids: None,
                created_at: None,
            }),
            sort_by: None,
            sort_order: None,
        };

        // 纯内存模式:name 过滤忽略（warn）,全量返回
        let response = service.query_apps(by_name("alpha")).await.expect("query");
        assert_eq!(response.items.len(), 2, "memory mode ignores name filter");

        // 注入持久化 + 元数据:过滤生效
        let persistence = InMemoryMetadataPersistence::new(vec![
            AppMetadataRecord {
                app_id: "app-alpha".into(),
                name: Some("alpha".into()),
                user_id: None,
                tenant_id: None,
                space_id: None,
                created_at: chrono::Utc::now() - chrono::Duration::hours(2),
            },
            AppMetadataRecord {
                app_id: "app-beta".into(),
                name: Some("beta".into()),
                user_id: None,
                tenant_id: None,
                space_id: None,
                created_at: chrono::Utc::now(),
            },
        ]);
        service.set_metadata_persistence(persistence.clone());
        service.apply_metadata_loaded(persistence.load_all().await.expect("load"));

        let response = service.query_apps(by_name("alpha")).await.expect("query");
        assert_eq!(response.items.len(), 1, "name filter now effective");
        assert_eq!(response.items[0].app_id, "app-alpha");

        // created_at range:只含 2 小时前创建的 alpha
        let now = chrono::Utc::now();
        let response = service
            .query_apps(QueryAppsRequest {
                page: None,
                page_size: None,
                filters: Some(AppFilters {
                    status: None,
                    name: None,
                    app_ids: None,
                    created_at: Some(DateRange {
                        start: (now - chrono::Duration::hours(3)).to_rfc3339(),
                        end: (now - chrono::Duration::hours(1)).to_rfc3339(),
                    }),
                }),
                sort_by: None,
                sort_order: None,
            })
            .await
            .expect("query by range");
        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].app_id, "app-alpha");
    }
}
