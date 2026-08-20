//! 应用管理服务层（统一 Docker / K8s 后端，无状态）
//!
//! rcoder 是无状态的应用 pod 引擎：
//! - 写操作（create/start/stop/restart/delete）转调 [`ContainerRuntime`] 的 Deployment 能力；
//! - 读操作（get/query/list）实时查集群，返回 [`AppRuntimeInfo`]；
//! - 业务元数据（name/image/command/env 等）由调用方（Java）持久化，rcoder 不存。
//!
//! K8s 模式 `create_deployment` 创建 ConfigMap/Secret/ClusterIP Service/Deployment；
//! HTTP 入口按 `http_expose`：Pingora（默认，两后端统一，本服务注册 Pingora backend
//! `/proxy/apps/{app_id}/{port}` → 后端 host：Docker container_ip / K8s ClusterIP FQDN）或 Gateway
//! （可选，K8s 建 HTTPRoute `/apps/{id}`）。TCP 初期不对外。Docker 模式建容器入主网络。

use std::sync::Arc;

use dashmap::DashMap;
use docker_manager::path::HostPathResolver;
use moka::sync::Cache;
use tracing::{info, instrument, warn};

use container_runtime_api::{ExposeType as RtExposeType, HttpExpose, UserAppRuntime};
use rcoder_proxy::PingoraProxyService;

use crate::AppActivityRegistry;
use crate::app_metadata::AppMetadataStore;

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
    pub(crate) path_resolver: Cache<String, Arc<HostPathResolver>>,
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
}

impl AppService {
    /// 创建新的应用管理服务
    pub async fn new(
        config: AppManagerConfig,
        runtime: Arc<dyn UserAppRuntime>,
        activity: Arc<AppActivityRegistry>,
        pingora: Option<Arc<PingoraProxyService>>,
    ) -> AppResult<Self> {
        let path_resolver: Cache<String, Arc<HostPathResolver>> =
            Cache::builder().max_capacity(1).build();

        // 初始化路径解析器（失败不致命，Docker 模式回退到容器内路径）
        match HostPathResolver::new().await {
            Ok(resolver) => {
                info!("[APP] path resolver initialized");
                path_resolver.insert("default".to_string(), Arc::new(resolver));
            }
            Err(e) => {
                warn!(
                    "[APP] path resolver init failed, using container path: {}",
                    e
                );
            }
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
            path_resolver,
            pingora_ports: DashMap::new(),
            release_locks: DashMap::new(),
            metadata: AppMetadataStore::default(),
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

    /// 对账接口：列出集群中所有 rcoder 托管的应用运行时状态
    #[instrument(skip(self))]
    pub async fn list_app_runtimes(&self) -> AppResult<Vec<AppRuntimeInfo>> {
        let statuses = self
            .runtime
            .list_deployments()
            .await
            .map_err(|e| map_runtime_error("[APP] list_deployments failed", e))?;
        Ok(statuses
            .into_iter()
            .map(|s| self.build_runtime_info(s))
            .collect())
    }

    /// 查询应用列表（实时查集群 + 过滤/分页）
    #[instrument(skip(self, request))]
    pub async fn query_apps(
        &self,
        request: QueryAppsRequest,
    ) -> AppResult<PaginatedResponse<AppRuntimeInfo>> {
        let mut items = self.list_app_runtimes().await?;

        // 过滤：status/app_ids 为运行时字段直接生效；name/created_at 需业务元数据
        // （集群不持有），仅 PG 模式（metadata 持久化已注入）经内存 join 生效，
        // 纯内存模式维持忽略 + warn（旧行为）。
        if let Some(filters) = &request.filters {
            if let Some(status) = &filters.status {
                items.retain(|app| status.contains(&app.status));
            }
            if let Some(app_ids) = &filters.app_ids {
                items.retain(|app| app_ids.contains(&app.app_id));
            }
            if filters.name.is_some() || filters.created_at.is_some() {
                if self.metadata.persistence().is_some() {
                    let name = filters.name.as_deref();
                    // DateRange RFC3339 解析失败 → 400（过滤已生效，非法参数应被告知）
                    let range = match &filters.created_at {
                        Some(range) => {
                            let start = chrono::DateTime::parse_from_rfc3339(&range.start)
                                .map_err(|e| {
                                    AppOperationError::Validation(format!(
                                        "invalid created_at.start '{}': {e}",
                                        range.start
                                    ))
                                })?
                                .with_timezone(&chrono::Utc);
                            let end = chrono::DateTime::parse_from_rfc3339(&range.end)
                                .map_err(|e| {
                                    AppOperationError::Validation(format!(
                                        "invalid created_at.end '{}': {e}",
                                        range.end
                                    ))
                                })?
                                .with_timezone(&chrono::Utc);
                            Some((start, end))
                        }
                        None => None,
                    };
                    items.retain(|app| {
                        let Some(meta) = self.metadata.lookup(&app.app_id) else {
                            // 无元数据记录的 app（非 PG 时代创建）不满足 name/created_at 过滤
                            return false;
                        };
                        // name 模糊匹配（contains，与 models 注释"按名称模糊搜索"对齐；
                        // 此前精确匹配导致部分名称查询恒 0 条且无提示）
                        name.is_none_or(|n| meta.name.as_deref().is_some_and(|v| v.contains(n)))
                            && range.is_none_or(|(start, end)| {
                                meta.created_at >= start && meta.created_at <= end
                            })
                    });
                } else {
                    warn!(
                        "[APP] query_apps name/created_at filters require business metadata (PG mode), ignored"
                    );
                }
            }
        }

        // 排序（app_id 直接可用；name/created_at 经 metadata join，缺元数据排最后；默认升序）
        if let Some(sort_by) = &request.sort_by {
            match sort_by.as_str() {
                "app_id" => {
                    items.sort_by(|a, b| a.app_id.cmp(&b.app_id));
                }
                "name" => {
                    if self.metadata.persistence().is_none() {
                        warn!("[APP] sort_by=name requires business metadata (PG mode), no-op");
                    }
                    items.sort_by_key(|app| {
                        self.metadata
                            .lookup(&app.app_id)
                            .and_then(|m| m.name)
                            .unwrap_or_default()
                    });
                }
                "created_at" => {
                    if self.metadata.persistence().is_none() {
                        warn!(
                            "[APP] sort_by=created_at requires business metadata (PG mode), no-op"
                        );
                    }
                    // (缺元数据排最后, 时间升序)：bool false < true 保证有元数据的排前
                    items.sort_by_key(|app| {
                        let meta = self.metadata.lookup(&app.app_id);
                        (meta.is_none(), meta.map(|m| m.created_at))
                    });
                }
                // 非法值 400（此前落入 `_ => {}` 不排序，但随后的 reverse 仍执行——
                // 传 created_at 等未支持值+desc 会把默认顺序直接反转，半生效的静默错误）
                other => {
                    return Err(AppOperationError::Validation(format!(
                        "sort_by must be one of app_id/name/created_at, got '{other}'"
                    )));
                }
            }
            if request.sort_order == Some(SortOrder::Desc) {
                items.reverse();
            }
        }

        // 分页（对齐 query_storage/publish tasks 的校验口径：非法值 400 而非静默 clamp——
        // 此前 page 超大在 debug 构建 u32 乘法溢出 panic、release 环绕返回错页数据；
        // page_size=0 算出 total_pages=42 亿）
        let page = request.page.unwrap_or(1);
        let page_size = request.page_size.unwrap_or(20);
        if page < 1 {
            return Err(AppOperationError::Validation(
                "page must be >= 1".to_string(),
            ));
        }
        if !(1..=100).contains(&page_size) {
            return Err(AppOperationError::Validation(
                "page_size must be within 1..=100".to_string(),
            ));
        }
        let total = items.len() as u64;
        // u64 中间量防溢出（合法输入下 (page-1)*page_size 最大 ~4.3e11，超 usize 的
        // 极端页码截断为越界空页而非 panic/环绕）
        let start = ((page as u64 - 1) * page_size as u64) as usize;
        let end = (start + page_size as usize).min(items.len());
        let paged_items = if start < items.len() {
            items[start..end].to_vec()
        } else {
            vec![]
        };

        Ok(PaginatedResponse {
            items: paged_items,
            pagination: Pagination {
                page,
                page_size,
                total,
                total_pages: ((total as f64) / (page_size as f64)).ceil() as u32,
            },
        })
    }

    /// 获取应用运行时详情（实时查集群；精确区分 404 与 500）
    #[instrument(skip(self))]
    pub async fn get_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        let status = self.fetch_runtime_status_or_err(app_id).await?;
        Ok(self.build_runtime_info(status))
    }

    /// 更新应用配置
    /// 更新应用（v2 §5.2，全量替换 desired state）。
    ///
    /// rcoder 无状态：不持有旧 desired state，故本操作为**全量替换**——调用方需发送完整
    /// 新状态（`image` 必填）。K8s 走 SSA re-apply（幂等）+ orphan 端口/配置清理；
    /// Docker 重建容器（image/env/command 变化必须重建），工作空间目录保留。
    #[instrument(skip(self, request))]
    pub async fn update_app(
        &self,
        app_id: &str,
        request: UpdateAppRequest,
    ) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        // 与发布串行（同 create/delete 的 per-app 进程级发布锁），但**不排队傻等**——
        // activate 等就绪可达 30 分钟，update 等它没有意义；锁被占（发布进行中）立即
        // 409 让调用方稍后重试。delete 保持阻塞等待语义（清理动作，等一下无妨）。
        // 无并发发布时锁条目可能不存在 → entry 建立并立刻拿到（try 必成功）。
        let lock_arc = match self.release_locks.entry(app_id.to_owned()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                entry.insert(lock.clone());
                lock
            }
        };
        let _update_lock = lock_arc.try_lock_owned().map_err(|_| {
            AppOperationError::Conflict(format!(
                "app {app_id} is being activated/published, retry after it finishes"
            ))
        })?;
        let current = self.fetch_runtime_status_or_err(app_id).await?;
        // 乐观锁：expected_resource_version 不匹配 → 409 Conflict
        // （Docker resource_version=None → 跳过校验，开发环境 last-write-wins 可接受）
        if let Some(expected) = &request.expected_resource_version
            && let Some(actual) = &current.resource_version
            && expected != actual
        {
            return Err(AppOperationError::Conflict(format!(
                "resource version mismatch: expected={expected}, actual={actual}"
            )));
        }
        let params = self
            .build_container_params_from_update(app_id, &request, &current)
            .await?;
        // 恢复依据先取出（unregister 会移除注册表条目）：pingora_ports 里的是当前
        // 实际生效的 Http 端口——比 current.ports 反推可靠（Docker 后端的状态 ports
        // 只含 TCP，反推恒空会让恢复分支注册了个寂寞）。
        let registered_http_ports = self.registered_http_ports(app_id);
        // 先注销旧 Pingora backend（K8s/Docker 都执行：Docker 旧 container_ip 失效；
        // K8s 下方按本次 http_ports 重新注册到 Service FQDN，注销-重注成对保证一致）。
        self.unregister_pingora_backends(app_id).await;
        // http_ports 在 move 前从 params 提取：优先本次回退后的完整 ports（live 回退
        // 后含全部端口的权威 desired）；读失败降级（params.ports=None）时退当前注册值。
        let http_ports: Vec<u16> = params
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .filter(|p| matches!(p.expose_type, RtExposeType::Http))
                    .map(|p| p.port)
                    .collect()
            })
            .unwrap_or(registered_http_ports);
        let info = match self.runtime.patch_deployment(params).await {
            Ok(info) => info,
            Err(e) => {
                // patch 失败：Deployment 原样仍在运行，恢复 pingora 路由（对齐 delete_app
                // 的失败恢复分支）——否则应用还在跑但 /proxy/apps/{id}/{port} 404，直到
                // 下次成功 update 或进程重启。
                let previous_host = current.pod_ip.clone().unwrap_or_default();
                self.register_pingora_backends(app_id, &http_ports, &previous_host)
                    .await;
                return Err(map_runtime_error(
                    &format!("[APP] patch_deployment failed app_id={app_id}"),
                    e,
                ));
            }
        };
        // 重新注册 Pingora backend（与上面 unregister 对称——否则部分更新会丢
        // Pingora 路由，app 经 /proxy/apps/{id}/{port} 变 502）。
        // 注：register 在 K8s 模式并非 no-op，会把 backend 指到 Service FQDN（与 create 一致）。
        self.register_pingora_backends(app_id, &http_ports, &info.container_ip)
            .await;
        info!("[APP] app updated: {}", app_id);
        // 业务元数据 upsert（created_at SQL 侧不更新）。name 缺省回退已存值——
        // update 语义里 name 是"仅元数据"调用方常不带,upsert 是整字段覆盖,
        // 直传 None 会把业务名清空（query name 过滤随之失效）。tenant/space
        // 保持与 label 相同的"携带即覆盖"语义（create 时的值不回退）。
        let name = request
            .name
            .clone()
            .or_else(|| self.metadata.lookup(app_id).and_then(|meta| meta.name));
        self.metadata
            .record(
                app_id,
                name,
                request.tenant_id.clone(),
                request.space_id.clone(),
            )
            .await;
        drop(_update_lock);
        self.remove_unused_process_release_lock(app_id);
        self.get_app(app_id).await
    }

    /// 删除应用（v2 §5.3：默认保留持久存储，purge=true 才清空数据面）。
    #[instrument(skip(self))]
    pub async fn delete_app(
        &self,
        app_id: &str,
        purge: bool,
        expected_resource_version: Option<&str>,
    ) -> AppResult<()> {
        validate_app_id(app_id)?;
        let previous = self.fetch_runtime_status_or_err(app_id).await?;
        let previous_wake_on_traffic = previous
            .wake_on_traffic
            .unwrap_or_else(|| !self.activity.is_wake_blocked(app_id));
        // 乐观锁（同 update_app）：expected 不匹配 → 409 Conflict
        if let Some(expected) = expected_resource_version
            && let Some(actual) = &previous.resource_version
            && expected != actual
        {
            return Err(AppOperationError::Conflict(format!(
                "resource version mismatch: expected={expected}, actual={actual}"
            )));
        }
        // delete/purge 必须与 prepare/activate/confirm/delete-release 串行，避免删除 PVC
        // 时另一个任务仍在写版本包或切换 code。
        let release_lock = self.acquire_process_release_lock(app_id).await;
        info!("[APP] deleting app: {} (purge={})", app_id, purge);

        // 1. Docker 模式：清理 Pingora backend（恢复依据先取出——unregister 会移除
        //    注册表条目；用注册表而非 previous.ports 反推，Docker 后端的状态 ports
        //    只含 TCP、反推恒空）
        let registered_http_ports = self.registered_http_ports(app_id);
        self.unregister_pingora_backends(app_id).await;

        // 2. 删除计算资源（K8s: Deployment/Service/HTTPRoute/NodePort/ConfigMap/Secret
        //    + label orphan 扫描兜底；Docker: 容器）。持久存储默认保留。
        // 先阻止并发流量唤醒；删除失败时恢复原活动状态。
        self.activity.mark_wake_blocked(app_id);
        if let Err(error) = self.runtime.delete_deployment(app_id).await {
            self.restore_activity_state(app_id, &previous, previous_wake_on_traffic);
            self.register_pingora_backends(
                app_id,
                &registered_http_ports,
                previous.pod_ip.as_deref().unwrap_or_default(),
            )
            .await;
            return Err(map_runtime_error(
                &format!("[APP] delete_deployment failed app_id={app_id}"),
                error,
            ));
        }
        self.activity.forget_app(app_id);

        // 3. purge=true 必须销毁持久存储（K8s: PVC + Ceph subvolume；Docker:
        //    workspace 目录），与 API 的“全部删除”语义一致。仅清空目录却保留 PVC
        //    会继续占用配额，并让成功响应与实际状态不一致。
        //    元数据行**保留**（三档语义：delete/purge 保留行支持误删找回，仅独立
        //    storage/destroy 接口删行）。
        if purge {
            self.destroy_app_storage_keep_metadata(app_id, app_id)
                .await?;
            info!("[APP] persistent storage destroyed: {}", app_id);
        } else {
            info!(
                "[APP] retained persistent storage (pass purge=true to clear): {}",
                app_id
            );
        }

        drop(release_lock);
        self.remove_unused_process_release_lock(app_id);
        Ok(())
    }
}

#[async_trait::async_trait]
impl super::AppServiceTrait for AppService {
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

    async fn reset_db_password(
        &self,
        app_id: &str,
        request: ResetDbPasswordRequest,
    ) -> AppResult<()> {
        self.reset_db_password(app_id, request).await
    }

    async fn create_database(&self, app_id: &str, request: CreateDatabaseRequest) -> AppResult<()> {
        self.create_database(app_id, request).await
    }

    async fn query_storage(
        &self,
        request: QueryStorageRequest,
    ) -> AppResult<PaginatedResponse<StorageInfo>> {
        self.query_storage(request).await
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

    async fn prepare_release(
        &self,
        app_id: &str,
        request: PrepareReleaseRequest,
    ) -> AppResult<ReleaseInfo> {
        self.prepare_release(app_id, request).await
    }

    async fn activate_release(
        &self,
        app_id: &str,
        release_id: &str,
        readiness_timeout: Option<u64>,
    ) -> AppResult<ReleaseInfo> {
        self.activate_release(app_id, release_id, readiness_timeout)
            .await
    }

    async fn rollback_release(
        &self,
        app_id: &str,
        message: Option<String>,
    ) -> AppResult<ReleaseInfo> {
        self.rollback_release(app_id, message).await
    }

    async fn list_releases(&self, app_id: &str) -> AppResult<ReleaseListResponse> {
        self.list_releases(app_id).await
    }

    async fn delete_release(&self, app_id: &str, release_id: &str) -> AppResult<()> {
        self.delete_release(app_id, release_id).await
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

    use crate::test_support::{MockRuntime, release_lock, test_service};

    fn create_request(app_id: &str) -> CreateAppRequest {
        CreateAppRequest {
            app_id: Some(app_id.to_owned()),
            name: "r2-app".into(),
            image: "registry.example/app-runtime:test".into(),
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
    async fn create_app_runtime_failure_triggers_best_effort_cleanup() {
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
    async fn create_app_cleanup_failure_keeps_original_error() {
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
    async fn update_app_without_name_keeps_metadata_name() {
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
    async fn update_app_conflicts_while_release_lock_held() {
        let root = tempfile::tempdir().expect("tempdir");
        let service = test_service(root.path(), Arc::new(MockRuntime::default()));
        let _publish_lock = service.acquire_process_release_lock("app-busy").await;

        let request = UpdateAppRequest {
            name: None,
            image: Some("registry.example/app-runtime:v2".into()),
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

    /// query_apps 分页校验：page<1 / page_size∉[1,100] → 400（对齐 query_storage 与
    /// publish tasks 口径；此前静默 clamp，超大 page 在 debug 构建乘法溢出 panic）。
    #[tokio::test]
    async fn query_apps_rejects_invalid_pagination_and_sort() {
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
    async fn delete_app_purge_keeps_metadata_row_until_explicit_destroy() {
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
    async fn query_apps_name_filter_respects_metadata_mode() {
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
                tenant_id: None,
                space_id: None,
                created_at: chrono::Utc::now() - chrono::Duration::hours(2),
            },
            AppMetadataRecord {
                app_id: "app-beta".into(),
                name: Some("beta".into()),
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
