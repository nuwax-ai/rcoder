//! 应用管理服务层（统一 Docker / K8s 后端，无状态）
//!
//! rcoder 是无状态的应用 pod 引擎：
//! - 写操作（create/start/stop/restart/delete）转调 [`ContainerRuntime`] 的 Deployment 能力；
//! - 读操作（get/query/list）实时查集群，返回 [`AppRuntimeInfo`]；
//! - 业务元数据（name/image/command/env 等）由调用方（Java）持久化，rcoder 不存。
//!
//! K8s 模式 `create_deployment` 创建 ConfigMap/Secret/ClusterIP Service/Deployment；
//! HTTP 入口按 `http_expose`：Pingora（默认，两后端统一，本服务注册 Pingora backend
//! `/proxy/{port}` → 后端 host：Docker container_ip / K8s ClusterIP FQDN）或 Gateway
//! （可选，K8s 建 HTTPRoute `/apps/{id}`）。TCP 初期不对外。Docker 模式建容器入主网络。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use dashmap::DashMap;
use docker_manager::path::HostPathResolver;
use moka::sync::Cache;
use tokio::fs;
use tracing::{info, instrument, warn};
use uuid::Uuid;

use container_runtime_api::{
    AppHealthCheck, AppPortSpec, AppResourceRequirements, ContainerCreateParams, ContainerRuntime,
    ContainerRuntimeError, DeploymentStatus, ExposeType as RtExposeType,
    HealthCheckType as RtHealthCheckType, HttpExpose,
};
use download_utils::{
    ArchiveError, detect_file_type, extract_tar_gz, extract_zip, normalize_extracted_dir,
};
use rcoder_proxy::PingoraProxyService;
use shared_types::ServiceType;

use super::config::{AppAccessMode, AppManagerConfig};
use super::models::*;

/// 应用管理服务（Docker / K8s 统一）
pub struct AppService {
    config: AppManagerConfig,
    runtime: Arc<dyn ContainerRuntime>,
    /// Pingora 代理（Docker 模式用于注册 HTTP backend；K8s 模式通常为 None）
    pingora: Option<Arc<PingoraProxyService>>,
    /// 路径解析器缓存（单例；Docker 模式将 rcoder 容器内路径解析为宿主机路径）
    path_resolver: Cache<String, Arc<HostPathResolver>>,
    /// Docker 模式 Pingora backend 端口登记（app_id → 注册的 HTTP 端口列表）
    ///
    /// 这是**操作副作用的临时缓存**（非业务元数据）：delete 时需要知道曾注册过哪些端口
    /// 才能清理 Pingora backend。rcoder 重启后丢失可接受（Docker 模式定位为开发环境）。
    pingora_ports: DashMap<String, Vec<u16>>,
}

impl AppService {
    /// 创建新的应用管理服务
    pub async fn new(
        config: AppManagerConfig,
        runtime: Arc<dyn ContainerRuntime>,
        pingora: Option<Arc<PingoraProxyService>>,
    ) -> AppResult<Self> {
        let path_resolver: Cache<String, Arc<HostPathResolver>> =
            Cache::builder().max_capacity(1).build();

        // 初始化路径解析器（失败不致命，Docker 模式回退到容器内路径）
        match HostPathResolver::new().await {
            Ok(resolver) => {
                info!("[APP] 路径解析器初始化成功");
                path_resolver.insert("default".to_string(), Arc::new(resolver));
            }
            Err(e) => {
                warn!("[APP] 路径解析器初始化失败，将使用容器内路径: {}", e);
            }
        }

        // K8s 模式：启动校验前置条件（RBAC 等）。失败 log warn 不阻塞（Fail Fast 暴露部署侧
        // RBAC 缺失，而非运行时创建 app 才 403）。Docker 模式 trait 默认 Ok，跳过。
        if config.access_mode == AppAccessMode::Kubernetes {
            match runtime.validate_app_prerequisites().await {
                Ok(_) => info!("[APP] K8s 前置校验通过（RBAC/apps/deployments 可访问）"),
                Err(e) => warn!("[APP] K8s 启动前置校验失败，app 管理可能无法工作: {}", e),
            }
        }

        // 无效组合告警（Fail Fast）：Docker 无 HTTPRoute/gateway 概念，gateway 模式不可用。
        // 不阻塞启动（便于临时切回 pingora），但 HTTP 将不可访问。
        if config.access_mode == AppAccessMode::Docker && config.http_expose == HttpExpose::Gateway
        {
            warn!(
                "[APP] 无效组合 access_mode=docker + http_expose=gateway：Docker 无 HTTPRoute，gateway 模式不可用，HTTP 将不可访问；请设 RCODER_APP_HTTP_EXPOSE=pingora"
            );
        }

        let svc = Self {
            config,
            runtime,
            pingora,
            path_resolver,
            pingora_ports: DashMap::new(),
        };
        // K8s Pingora 模式：启动时从集群重建 Pingora backends——修复 pingora_ports 内存态
        // 丢失导致的重启 silent 404（list_deployments 的 expose_type 已由 Deployment annotation
        // 准确还原）。失败不阻塞启动（warn，待下次 create/update 恢复）。
        if svc.config.access_mode == AppAccessMode::Kubernetes
            && svc.config.http_expose == HttpExpose::Pingora
            && let Err(e) = svc.rebuild_pingora_backends().await
        {
            warn!(
                "[APP] Pingora backends 重建失败（重启后 HTTP 暂不可达，待下次 create/update 恢复）: {}",
                e
            );
        }
        Ok(svc)
    }

    /// 创建应用
    #[instrument(skip(self, request))]
    pub async fn create_app(&self, request: CreateAppRequest) -> AppResult<AppInfo> {
        // app_id：外部指定（app- + DNS-1123，校验 + 唯一性）or 自动生成
        let app_id = match &request.app_id {
            Some(id) => {
                validate_app_id(id)?;
                // 唯一性：已存在 → ERR_APP_ALREADY_EXISTS（防止 SSA force=true 静默覆盖）
                if let Ok(Some(_)) = self.runtime.get_deployment_status(id).await {
                    return Err(AppOperationError::AlreadyExists(format!(
                        "app already exists: {id}"
                    )));
                }
                id.clone()
            }
            None => format!("app-{}", &Uuid::new_v4().to_string()[..8]),
        };
        info!(
            "[APP] 创建应用: {} ({}, mode={:?})",
            request.name, app_id, self.config.access_mode
        );

        // 0. 校验资源限制格式（K8s Quantity: storage / ephemeral_storage）→ ERR_VALIDATION
        if let Some(ref resources) = request.resources {
            if let Some(ref s) = resources.storage {
                crate::handler::pod_handler::validate_k8s_storage_size(s).map_err(|e| {
                    AppOperationError::Validation(format!("invalid storage '{}': {}", s, e))
                })?;
            }
            if let Some(ref es) = resources.ephemeral_storage {
                crate::handler::pod_handler::validate_k8s_storage_size(es).map_err(|e| {
                    AppOperationError::Validation(format!(
                        "invalid ephemeral_storage '{}': {}",
                        es, e
                    ))
                })?;
            }
        }

        // 0.5 校验端口：HTTP 端口 ≤ 1（access 只报单个 path；gateway 模式 HTTPRoute 也仅支持单 HTTP 端口）
        let http_port_count = request
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .filter(|p| p.expose_type == ExposeType::Http)
                    .count()
            })
            .unwrap_or(0);
        if http_port_count > 1 {
            return Err(AppOperationError::Validation(format!(
                "at most 1 HTTP port allowed (got {http_port_count}); multi HTTP port support pending"
            )));
        }
        // 0.5b 端口号唯一：避免 K8s annotation 解码歧义（同 port 不同 type 会被 HashMap 折叠）
        // 及 Pingora backend key(port) 冲突。Fail Fast 在源头拒绝。
        if let Some(ports) = &request.ports {
            let mut seen = std::collections::HashSet::new();
            for p in ports {
                if !seen.insert(p.port) {
                    return Err(AppOperationError::Validation(format!(
                        "port {} duplicate: each port number must be unique",
                        p.port
                    )));
                }
            }
        }

        // 1. K8s: ensure per-app PVC 带用户配额 requests.storage + 等 subvolumePath 就绪。Docker no-op。
        //    必须在 create_app_dirs (建目录) + create_deployment (Docker bind mount 需源目录存在) 之前:
        //    首次 ensure 带配额, 避免 create_deployment 内 ensure 命中 active 复用丢配额。
        let storage_size = request.resources.as_ref().and_then(|r| r.storage.as_deref());
        self.ensure_app_workspace_ready(&app_id, storage_size).await?;

        // 2. 创建应用工作空间目录（code/data/logs）—— Docker: 共享 Local (create_deployment bind mount 源,
        //    必须先存在); K8s: per-app PVC 根 (ensure_app_workspace_ready 已 ensure + 等 subvolumePath)。
        self.create_app_dirs(&app_id).await?;

        // 3. 构建容器创建参数（UserApp）
        let params = self.build_container_params(&app_id, &request).await?;

        // 4. 创建 Deployment / 容器（K8s 含 ConfigMap/Secret/Service/HTTPRoute/NodePort;
        //    PVC active 复用 / Docker bind mount 共享目录已存在）。
        let container_info = self.runtime.create_deployment(params).await.map_err(|e| {
            map_runtime_error(
                &format!("[APP] create_deployment failed app_id={app_id}"),
                e,
            )
        })?;
        info!(
            "[APP] 应用资源创建成功: {} (container={})",
            app_id, container_info.container_name
        );

        // 注: UserApp 是新开发逻辑 (application-management-service-v2-design.md), /app 路径
        // 不涉及历史数据迁移 → 不调 lazy_migrate (新应用无旧数据)。
        // Web/Computer 有历史数据 → 保留 lazy_migrate。

        // 4. Docker 模式：为 HTTP 端口注册 Pingora backend（/proxy/{port} → container_ip）
        let http_ports = http_port_numbers(&request.ports);
        self.register_pingora_backends(&app_id, &http_ports, &container_info.container_ip)
            .await;

        // 5. 实时查询运行时状态（K8s 用于拿真实 node_port；Docker 模式不还原端口语义）
        let runtime_status = self.fetch_runtime_status(&app_id).await;

        // 端口状态：以请求端口为准（expose_type 语义完整），合并运行时返回的 external_port
        // （K8s node_port）。Docker 模式 get_deployment_status 不还原端口语义，Tcp 的 host_port
        // 留空（已知限制：Docker Tcp 对外端口需通过 docker inspect port_bindings 另查）。
        let mut ports: Vec<AppPortStatus> = request
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(|p| AppPortStatus {
                        name: p.name.clone(),
                        port: p.port,
                        expose_type: map_expose_type(&p.expose_type),
                        external_port: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(status) = &runtime_status {
            for rt_p in &status.ports {
                let Some(ep) = rt_p.external_port else {
                    continue;
                };
                // 按 port 匹配 external_port（Docker get_deployment_status 的 name 是
                // tcp-{port}，与请求 name 不一致；port 唯一，K8s/Docker 通用）
                if let Some(ap) = ports.iter_mut().find(|p| p.port == rt_p.port) {
                    ap.external_port = Some(ep);
                }
            }
        }

        // 6. 构建访问信息 + 健康信息
        let access = self.build_access_info(&app_id, &ports);
        let health = runtime_status
            .as_ref()
            .map(health_from_status)
            .unwrap_or(HealthInfo {
                status: "Unknown".to_string(),
                instance: None,
                probes: None,
            });

        // status：用刚查到的运行时 phase 映射（不再硬编码 Running）——刚创建的 Pod 通常
        // 还是 Starting，甚至镜像拉取失败已 Error；返回真实状态避免"status=Running 但
        // health=Starting/Error"自相矛盾。message 带 phase=Error 的失败原因。
        let (status, message) = match &runtime_status {
            Some(s) => (phase_to_status(&s.phase), s.message.clone()),
            None => (AppStatus::Starting, None),
        };

        let now = Utc::now().to_rfc3339();
        Ok(AppInfo {
            app_id: app_id.clone(),
            name: request.name.clone(),
            status,
            message,
            image: request.image.clone(),
            command: request.command.clone().unwrap_or_default(),
            replicas: 1,
            access,
            health,
            resources: request.resources.clone(),
            env: request.env.clone().unwrap_or_default(),
            created_at: now.clone(),
            updated_at: now,
        })
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

        // 过滤（仅 status/app_ids 为运行时字段，可生效；name/created_at 需业务元数据，跳过）
        if let Some(filters) = &request.filters {
            if let Some(status) = &filters.status {
                items.retain(|app| status.contains(&app.status));
            }
            if let Some(app_ids) = &filters.app_ids {
                items.retain(|app| app_ids.contains(&app.app_id));
            }
            if filters.name.is_some() || filters.created_at.is_some() {
                warn!(
                    "[APP] query_apps 的 name/created_at 过滤需要业务元数据，rcoder 无状态已忽略"
                );
            }
        }

        // 排序（仅 app_id 可用；默认升序，Desc 时降序）
        if let Some(sort_by) = &request.sort_by
            && (sort_by == "app_id" || sort_by == "name")
        {
            items.sort_by(|a, b| a.app_id.cmp(&b.app_id));
            if request.sort_order == Some(SortOrder::Desc) {
                items.reverse();
            }
        }

        // 分页
        let total = items.len() as u64;
        let page = request.page.unwrap_or(1).max(1);
        let page_size = request.page_size.unwrap_or(20).min(100);
        let start = ((page - 1) * page_size) as usize;
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
        let params = self.build_container_params_from_update(app_id, &request).await?;
        // Docker 模式：旧容器 IP 即将失效，先注销 Pingora backend（K8s no-op）
        self.unregister_pingora_backends(app_id).await;
        let info = self.runtime.patch_deployment(params).await.map_err(|e| {
            map_runtime_error(&format!("[APP] patch_deployment failed app_id={app_id}"), e)
        })?;
        // Docker 模式：新容器 IP，重新注册 Pingora backend（K8s no-op）
        let http_ports = http_port_numbers(&request.ports);
        self.register_pingora_backends(app_id, &http_ports, &info.container_ip)
            .await;
        info!("[APP] 应用已更新: {}", app_id);
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
        // 乐观锁（同 update_app）：expected 不匹配 → 409 Conflict
        if let Some(expected) = expected_resource_version {
            let current = self.fetch_runtime_status_or_err(app_id).await?;
            if let Some(actual) = &current.resource_version
                && expected != actual
            {
                return Err(AppOperationError::Conflict(format!(
                    "resource version mismatch: expected={expected}, actual={actual}"
                )));
            }
        }
        info!("[APP] 删除应用: {} (purge={})", app_id, purge);

        // 1. Docker 模式：清理 Pingora backend
        self.unregister_pingora_backends(app_id).await;

        // 2. 删除计算资源（K8s: Deployment/Service/HTTPRoute/NodePort/ConfigMap/Secret
        //    + label orphan 扫描兜底；Docker: 容器）。持久存储默认保留。
        self.runtime.delete_deployment(app_id).await.map_err(|e| {
            map_runtime_error(
                &format!("[APP] delete_deployment failed app_id={app_id}"),
                e,
            )
        })?;

        // 3. 仅 purge=true 时清空持久存储（code/data/logs 目录）。
        //    默认保留：应用可重建，数据不可再生（v2 §5.3 数据安全）。
        if purge {
            let app_dir = self.get_container_app_dir(app_id).await?;
            // K8s per-agent: app_dir = per-app PVC 根, 清空内容不删根 (同 delete_app_storage)
            if app_dir.exists()
                && let Err(e) = Self::purge_dir_contents(&app_dir).await
            {
                warn!("[APP] purge 清理应用目录失败 {:?}: {}", app_dir, e);
            }
            info!("[APP] 已清空应用持久存储: {}", app_id);
        } else {
            info!(
                "[APP] 保留应用持久存储（如需清空传 purge=true）: {}",
                app_id
            );
        }

        Ok(())
    }

    // ===== 持久存储管理（v2 §5.4）=====
    // 删应用默认保留数据；这组接口让 Java 显式管理残留存储。
    // StorageInfo 不含 size_bytes——CephFS 上不能用 du（详见设计文档 §5.4）。

    /// 查询单个应用的持久存储状态（O(1) stat，不递归）。
    pub async fn get_app_storage(&self, app_id: &str) -> AppResult<StorageInfo> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let metadata = tokio::fs::metadata(&app_dir).await.ok();
        let exists = metadata.is_some();
        let modified_at = metadata
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
        let is_orphan = self.is_storage_orphan(app_id).await;
        Ok(StorageInfo {
            app_id: app_id.to_string(),
            exists,
            path: app_dir.to_string_lossy().to_string(),
            modified_at,
            is_orphan,
        })
    }

    /// 清空应用的持久存储。安全约束：仅当 app 计算资源已不存在时允许（否则 INVALID_STATE）。
    pub async fn delete_app_storage(&self, app_id: &str) -> AppResult<()> {
        validate_app_id(app_id)?;
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(_)) => {
                return Err(AppOperationError::InvalidState(format!(
                    "app {app_id} still exists, delete it before clearing storage (to avoid corrupting in-use data)"
                )));
            }
            Ok(None) => {}
            Err(e) => {
                warn!("[APP] 查询应用状态失败 app_id={}: {}", app_id, e);
                return Err(AppOperationError::Backend(format!(
                    "failed to query app status: {e}"
                )));
            }
        }
        let app_dir = self.get_container_app_dir(app_id).await?;
        // K8s per-agent: app_dir = per-app PVC 根 (ceph-csi subvol 根), 清空内容不删根
        // (删 subvol 根破坏 PV subvolumePath → pod 重启挂载异常)
        if app_dir.exists()
            && let Err(e) = Self::purge_dir_contents(&app_dir).await
        {
            return Err(map_io_error("failed to clear storage", e, false));
        }
        info!("[APP] 已清空应用存储: {}", app_id);
        Ok(())
    }

    /// 清空目录内容 (逐子项 remove), 保留目录本身。
    /// purge per-agent PVC 根 (ceph-csi subvol 根) 必须用此 —— `remove_dir_all` 删 subvol 根
    /// 会破坏 PV `csi.volumeAttributes.subvolumePath` (PVC 仍在但 subvol 路径不存在 → pod 重启挂载异常)。
    async fn purge_dir_contents(dir: &std::path::Path) -> std::io::Result<()> {
        let mut rd = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let p = entry.path();
            if p.is_dir() {
                tokio::fs::remove_dir_all(&p).await?;
            } else {
                tokio::fs::remove_file(&p).await?;
            }
        }
        Ok(())
    }

    /// 分页查询持久存储（强制分页，无全量模式）。
    /// 过滤：orphan_only、app_ids 生效；tenant_id/space_id 在无状态下不支持（rcoder 不持
    /// app→租户映射），提供则 warn 忽略。
    pub async fn query_storage(
        &self,
        request: QueryStorageRequest,
    ) -> AppResult<PaginatedResponse<StorageInfo>> {
        if request.page == 0 {
            return Err(AppOperationError::Validation(
                "page starts from 1".to_string(),
            ));
        }
        if request.page_size == 0 || request.page_size > 100 {
            return Err(AppOperationError::Validation(
                "page_size must be in 1..=100".to_string(),
            ));
        }
        let filters = request.filters.unwrap_or_default();
        if filters.tenant_id.is_some() || filters.space_id.is_some() {
            warn!(
                "[APP] query_storage 的 tenant_id/space_id 过滤在无状态下不支持（rcoder 不持 app→租户映射），已忽略"
            );
        }
        // 现有 app 集合（供 is_orphan），一次 list 调用
        let existing: std::collections::HashSet<String> = self
            .runtime
            .list_deployments()
            .await
            .map_err(|e| map_runtime_error("[APP] list_deployments failed", e))?
            .into_iter()
            .map(|s| s.app_id)
            .collect();
        // 阶段2 per-app PVC: app 数据在各自 per-app PVC (rcoder 经挂根聚合访问), workspace_root
        // 不再含 app 目录 → app 列表来自 list_deployments (existing, Deployment by label),
        // 不 read_dir(workspace_root)。orphan 检测 (read_dir 找不在 Deployment 的目录) 在
        // per-agent 模式失效 (数据跨 PVC); existing 内全为非 orphan。
        let mut entries: Vec<String> = existing.iter().cloned().collect();
        entries.sort();
        let app_ids_filter = filters.app_ids.as_deref();
        let filtered: Vec<String> = entries
            .into_iter()
            .filter(|app_id| {
                if let Some(ids) = app_ids_filter
                    && !ids.iter().any(|x| x == app_id)
                {
                    return false;
                }
                if filters.orphan_only.unwrap_or(false) && existing.contains(app_id) {
                    return false;
                }
                true
            })
            .collect();
        let total = filtered.len() as u64;
        let page = request.page as usize;
        let page_size = request.page_size as usize;
        let start = page.saturating_sub(1) * page_size;
        let paged: Vec<String> = filtered.into_iter().skip(start).take(page_size).collect();

        let mut items = Vec::with_capacity(paged.len());
        for app_id in paged {
            let is_orphan = !existing.contains(&app_id);
            // resolve 失败 (K8s per-app PVC 未就绪) 不中断整个列表: warn + 标记 not exist
            let (exists, path, modified_at) = match self.get_container_app_dir(&app_id).await {
                Ok(app_dir) => {
                    let metadata = tokio::fs::metadata(&app_dir).await.ok();
                    let modified_at = metadata
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
                    (metadata.is_some(), app_dir.to_string_lossy().to_string(), modified_at)
                }
                Err(e) => {
                    tracing::warn!(
                        "[APP] list storage resolve {} failed, mark not exist: {}",
                        app_id,
                        e
                    );
                    (false, String::new(), None)
                }
            };
            items.push(StorageInfo {
                app_id,
                exists,
                path,
                modified_at,
                is_orphan,
            });
        }
        let total_pages = if total == 0 {
            1
        } else {
            total.div_ceil(page_size as u64) as u32
        };
        Ok(PaginatedResponse {
            items,
            pagination: Pagination {
                page: request.page,
                page_size: request.page_size,
                total,
                total_pages,
            },
        })
    }

    /// 存储是否为孤儿（无对应运行应用）。Ok(None)=orphan；Ok(Some)/Err=非 orphan（保守）。
    async fn is_storage_orphan(&self, app_id: &str) -> bool {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(e) => {
                // 瞬时 API 错误保守视为"非 orphan"（避免误删在用数据），但落日志可见
                warn!(
                    "[APP] is_storage_orphan 查询状态失败 app_id={}: {}",
                    app_id, e
                );
                false
            }
        }
    }

    /// 启动应用（scale replicas = 1）
    #[instrument(skip(self))]
    pub async fn start_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime
            .scale_deployment(app_id, 1)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] scale_deployment failed app_id={app_id}"), e)
            })?;
        info!("[APP] 应用已启动 (scale=1): {}", app_id);
        self.get_app(app_id).await
    }

    /// 停止应用（scale replicas = 0）
    #[instrument(skip(self))]
    pub async fn stop_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime
            .scale_deployment(app_id, 0)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] scale_deployment failed app_id={app_id}"), e)
            })?;
        info!("[APP] 应用已停止 (scale=0): {}", app_id);
        self.get_app(app_id).await
    }

    /// 重启应用（rollout restart）
    #[instrument(skip(self))]
    pub async fn restart_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime.restart_deployment(app_id).await.map_err(|e| {
            map_runtime_error(
                &format!("[APP] restart_deployment failed app_id={app_id}"),
                e,
            )
        })?;
        info!("[APP] 应用已重启 (rollout): {}", app_id);
        self.get_app(app_id).await
    }

    /// 获取应用日志（实时拉容器 stdout/stderr：K8s Pod logs / docker logs）。
    ///
    /// `follow` 流式当前未实现（runtime 返回 tail 快照），`since` 暂未透传；
    /// SSE/WebSocket 实时流留待后续增强。
    #[instrument(skip(self))]
    pub async fn get_app_logs(&self, app_id: &str, params: LogParams) -> AppResult<Vec<LogEntry>> {
        validate_app_id(app_id)?;
        let tail = params.tail.unwrap_or(1000);
        let timestamps = params.timestamps.unwrap_or(true);
        let entries = self
            .runtime
            .get_app_logs(app_id, tail, timestamps)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] get_app_logs failed app_id={app_id}"), e)
            })?;
        Ok(entries
            .into_iter()
            .map(|e| LogEntry {
                timestamp: e.timestamp.unwrap_or_default(),
                stream: e.stream,
                message: e.message,
            })
            .collect())
    }

    /// 启动日志流（follow），返回 mpsc::Receiver 供 WS handler 桥接（v2 §11）。
    /// receiver drop 即取消：客户端断开 → handler 退出 → receiver 析构 → runtime 任务终止。
    pub async fn stream_app_logs(
        &self,
        app_id: &str,
        tail: u32,
    ) -> AppResult<container_runtime_api::mpsc::Receiver<container_runtime_api::ContainerLogEntry>>
    {
        validate_app_id(app_id)?;
        self.runtime
            .stream_app_logs(app_id, tail)
            .await
            .map_err(|e| {
                map_runtime_error(&format!("[APP] stream_app_logs failed app_id={app_id}"), e)
            })
    }

    /// 获取资源使用情况（best-effort：restart_count 来自运行时；CPU/内存需 metrics-server）
    #[instrument(skip(self))]
    pub async fn get_app_stats(&self, app_id: &str) -> AppResult<ResourceStats> {
        let status = self.fetch_runtime_status_or_err(app_id).await?;
        Ok(ResourceStats {
            restart_count: status.restart_count,
            ..Default::default()
        })
    }

    /// 获取应用事件（K8s Events API：调度/拉取/启动/崩溃）
    #[instrument(skip(self))]
    pub async fn get_app_events(
        &self,
        app_id: &str,
    ) -> AppResult<Vec<container_runtime_api::AppEventInfo>> {
        validate_app_id(app_id)?;
        self.runtime.get_app_events(app_id).await.map_err(|e| {
            map_runtime_error(&format!("[APP] get_app_events failed app_id={app_id}"), e)
        })
    }

    /// 读取应用文件日志（从 workspace PVC 的 logs/ 目录直接读，不依赖 K8s Pod log API）。
    ///
    /// 适用：不写 stdout 而写文件的应用（Java Spring Boot → logs/application.log 等）。
    /// 路径相对 app 根（如 "logs/app.log"），有 path traversal 防护。
    #[instrument(skip(self))]
    pub async fn get_app_file_logs(
        &self,
        app_id: &str,
        file_path: &str,
        tail: u32,
    ) -> AppResult<Vec<LogEntry>> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let target = app_dir.join(file_path);

        // path traversal 防护（与 upload/delete_file 一致）
        let canonical_target = match target.canonicalize() {
            Ok(c) => c,
            Err(_) => {
                return Err(AppOperationError::FileNotFound(format!(
                    "log file does not exist: {file_path}"
                )));
            }
        };
        let canonical_root = app_dir.canonicalize().unwrap_or_else(|_| app_dir.clone());
        if !canonical_target.starts_with(&canonical_root) {
            return Err(AppOperationError::Validation(format!(
                "path traversal rejected: {file_path}"
            )));
        }

        // 读文件，取最后 tail 行
        let content = tokio::fs::read_to_string(&canonical_target)
            .await
            .map_err(|e| {
                map_io_error(&format!("failed to read log file '{file_path}'"), e, true)
            })?;
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(tail as usize);
        Ok(lines[start..]
            .iter()
            .map(|line| LogEntry {
                timestamp: String::new(),
                stream: "file".to_string(),
                message: line.to_string(),
            })
            .collect())
    }

    /// 上传文件 / 压缩包。
    ///
    /// 自动判断（魔数）：zip/tar.gz 压缩包 → 解压到 `target` 目录；其它 → 单文件存 `target`。
    /// 单文件：`target`=文件路径（如 `code/app.jar`）；压缩包：`target`=解压目录（如 `code/`）。
    /// 安全：复用 download_utils 的 zip slip + 1GiB 大小防护，叠加 app 根 canonicalize 校验。
    #[instrument(skip(self, file_data))]
    pub async fn upload_file(
        &self,
        app_id: &str,
        file_data: Vec<u8>,
        target: &str,
        flatten: bool,
    ) -> AppResult<UploadResult> {
        validate_app_id(app_id)?;
        validate_upload_target(target)?; // create_dir_all 前拦截 ../ 与绝对路径（避免副作用泄漏）
        if file_data.is_empty() {
            return Err(AppOperationError::Validation(
                "file data is empty".to_string(),
            ));
        }
        let app_dir = self.get_container_app_dir(app_id).await?;
        fs::create_dir_all(&app_dir)
            .await
            .map_err(|e| map_io_error("failed to create app dir", e, false))?;
        let canonical_app_dir = app_dir
            .canonicalize()
            .map_err(|e| map_io_error("failed to resolve app dir", e, false))?;

        // 魔数判断压缩包类型（不靠文件名后缀，app.jar.zip 也能识别为 zip）
        let file_type = detect_file_type(&file_data);
        match file_type {
            "zip" | "tar.gz" => {
                self.extract_archive(
                    app_id,
                    file_data,
                    file_type,
                    target,
                    flatten,
                    &canonical_app_dir,
                )
                .await
            }
            _ => {
                // 单文件分支（target=文件路径，app 根相对）
                let file_path = app_dir.join(target);
                // 防穿越：canonicalize 父目录后校验仍在 app 目录内（与 delete_file 对称）
                if let Some(parent) = file_path.parent() {
                    fs::create_dir_all(parent)
                        .await
                        .map_err(|e| map_io_error("failed to create parent dir", e, false))?;
                    let canonical_parent = parent
                        .canonicalize()
                        .map_err(|e| map_io_error("failed to resolve parent dir", e, false))?;
                    if !canonical_parent.starts_with(&canonical_app_dir) {
                        return Err(AppOperationError::Validation(
                            "path is outside app dir".to_string(),
                        ));
                    }
                }
                fs::write(&file_path, &file_data)
                    .await
                    .map_err(|e| map_io_error("failed to write file", e, true))?;
                Ok(UploadResult {
                    file_path: target.to_string(),
                    file_size: file_data.len() as u64,
                    uploaded_at: Utc::now().to_rfc3339(),
                    extracted_count: None,
                })
            }
        }
    }

    /// 解压压缩包（zip/tar.gz）到 target 目录（app 根相对）。
    async fn extract_archive(
        &self,
        app_id: &str,
        file_data: Vec<u8>,
        file_type: &str,
        target: &str,
        flatten: bool,
        canonical_app_dir: &std::path::Path,
    ) -> AppResult<UploadResult> {
        let app_dir = self.get_container_app_dir(app_id).await?;
        let dest = app_dir.join(target.trim_end_matches('/'));
        fs::create_dir_all(&dest)
            .await
            .map_err(|e| map_io_error("failed to create extraction dir", e, false))?;
        let canonical_dest = dest
            .canonicalize()
            .map_err(|e| map_io_error("failed to resolve extraction dir", e, false))?;
        if !canonical_dest.starts_with(canonical_app_dir) {
            return Err(AppOperationError::Validation(
                "extraction dir is outside app dir".to_string(),
            ));
        }

        let file_size = file_data.len() as u64;
        let file_type = file_type.to_string(); // 由 upload_file 传入（避免重复 detect）
        let dest_clone = canonical_dest.clone();
        // spawn_blocking：写临时文件 + 解压（同步 IO，不阻塞 tokio；TempPath 闭包结束自动删）
        let count =
            tokio::task::spawn_blocking(move || -> std::result::Result<usize, ArchiveError> {
                let mut tmp = tempfile::NamedTempFile::new()?;
                tmp.write_all(&file_data)?;
                let tmp_path = tmp.into_temp_path();
                match file_type.as_str() {
                    "tar.gz" => extract_tar_gz(&tmp_path, &dest_clone),
                    "zip" => extract_zip(&tmp_path, &dest_clone),
                    _ => Err(ArchiveError::InvalidArchive(format!(
                        "unsupported: {file_type}"
                    ))),
                }
            })
            .await
            .map_err(|e| AppOperationError::Backend(format!("extraction task failed: {e}")))?
            .map_err(map_archive_error)?;

        if flatten {
            normalize_extracted_dir(&canonical_dest).map_err(map_archive_error)?;
        }
        info!(
            "[APP] 压缩包已解压: {} -> {} ({} 文件, flatten={})",
            app_id, target, count, flatten
        );
        Ok(UploadResult {
            file_path: target.to_string(),
            file_size,
            uploaded_at: Utc::now().to_rfc3339(),
            extracted_count: Some(count),
        })
    }

    /// 列出文件（app 根目录，或其子目录如 "code"/"data"/"logs"）。
    ///
    /// `subpath` 为 None/空 → 列 app 根；否则列 `app_dir/{subpath}`。返回的 `path` 字段是
    /// **app-root-relative**（如 "code/app.jar"），可直接作为 upload 的 target / delete 的 path，
    /// 与这两个接口的约定一致。防穿越：子目录 canonicalize 后必须仍在 app 目录内。
    #[instrument(skip(self))]
    pub async fn list_files(
        &self,
        app_id: &str,
        subpath: Option<&str>,
    ) -> AppResult<Vec<FileInfo>> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        if !app_dir.exists() {
            return Ok(vec![]);
        }
        let canonical_app_dir = app_dir
            .canonicalize()
            .map_err(|e| map_io_error("failed to resolve app dir", e, false))?;
        // subpath 归一化：去尾部 '/'，空 → 列 app 根
        let sub = subpath
            .map(|p| p.trim_end_matches('/'))
            .filter(|p| !p.is_empty());
        let target_dir = match sub {
            Some(p) => {
                let full = app_dir.join(p);
                if !full.exists() {
                    return Ok(vec![]);
                }
                let canonical_full = full
                    .canonicalize()
                    .map_err(|e| map_io_error("failed to resolve sub dir", e, false))?;
                if !canonical_full.starts_with(&canonical_app_dir) {
                    return Err(AppOperationError::Validation(
                        "path is outside app dir".to_string(),
                    ));
                }
                canonical_full
            }
            None => canonical_app_dir,
        };
        // 返回 app-root-relative 路径（sub 存在时前缀 "sub/"）
        let rel_prefix = sub.map(|p| format!("{p}/")).unwrap_or_default();
        let mut files = Vec::new();
        let mut entries = fs::read_dir(&target_dir)
            .await
            .map_err(|e| map_io_error("failed to read dir", e, false))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| map_io_error("failed to traverse dir", e, false))?
        {
            let metadata = entry
                .metadata()
                .await
                .map_err(|e| map_io_error("failed to read file metadata", e, false))?;
            files.push(FileInfo {
                path: format!("{rel_prefix}{}", entry.file_name().to_string_lossy()),
                size: metadata.len(),
                is_dir: metadata.is_dir(),
                modified_at: metadata
                    .modified()
                    .map(|t| {
                        let datetime: chrono::DateTime<Utc> = t.into();
                        datetime.to_rfc3339()
                    })
                    .unwrap_or_default(),
            });
        }
        Ok(files)
    }

    /// 删除文件
    #[instrument(skip(self))]
    pub async fn delete_file(&self, app_id: &str, file_path: &str) -> AppResult<()> {
        validate_app_id(app_id)?;
        // file_path 相对 app 根目录（与 upload_file 的 target 同约定：可指向 code/ data/ logs/）
        let app_dir = self.get_container_app_dir(app_id).await?;
        if !app_dir.exists() {
            return Err(AppOperationError::NotFound(format!(
                "app dir does not exist: {app_id}"
            )));
        }
        let full_path = app_dir.join(file_path);
        // 先 exists 守卫，避免 canonicalize 对不存在路径抛 OS 错误（导致 500 而非 404）
        if !full_path.exists() {
            return Err(AppOperationError::FileNotFound(format!(
                "file does not exist: {file_path}"
            )));
        }

        // 安全检查：canonicalize 后确保路径仍在 app 目录内（防 ../ 穿越到外部）
        let canonical_path = full_path
            .canonicalize()
            .map_err(|e| map_io_error("failed to resolve file path", e, false))?;
        let canonical_app_dir = app_dir
            .canonicalize()
            .map_err(|e| map_io_error("failed to resolve app dir", e, false))?;
        if !canonical_path.starts_with(&canonical_app_dir) {
            return Err(AppOperationError::Validation(
                "path is outside app dir".to_string(),
            ));
        }

        if canonical_path.is_dir() {
            fs::remove_dir_all(&canonical_path)
                .await
                .map_err(|e| map_io_error("failed to remove dir", e, false))?;
        } else {
            fs::remove_file(&canonical_path)
                .await
                .map_err(|e| map_io_error("failed to remove file", e, true))?;
        }
        info!("[APP] 文件已删除: {}", file_path);
        Ok(())
    }

    // ========================================================================
    // 辅助方法
    // ========================================================================

    /// 构建 ContainerCreateParams（UserApp）
    async fn build_container_params(
        &self,
        app_id: &str,
        request: &CreateAppRequest,
    ) -> AppResult<ContainerCreateParams> {
        // 端口：models::PortConfig → container_runtime_api::AppPortSpec
        let ports: Vec<AppPortSpec> = request
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(|p| AppPortSpec {
                        name: p.name.clone(),
                        port: p.port,
                        expose_type: map_expose_type(&p.expose_type),
                        strip_prefix: p.strip_prefix,
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Exec 健康检查当前未支持（AppHealthCheck 无 command 字段），Fail Fast 拒绝，
        // 避免静默丢弃用户配置（K8s build_probe 对 Exec 返回 None → 容器被视为永远健康）
        if let Some(hc) = &request.health_check
            && matches!(hc.check_type, HealthCheckType::Exec)
        {
            return Err(AppOperationError::Validation(
                "Exec health check is not supported (AppHealthCheck lacks command field); use Http/Tcp instead"
                    .to_string(),
            ));
        }

        // 健康检查：models::HealthCheckConfig → AppHealthCheck
        let health_check = request.health_check.as_ref().map(|hc| AppHealthCheck {
            check_type: map_health_check_type(&hc.check_type),
            path: hc.path.clone(),
            port: hc.port,
            initial_delay_seconds: None,
            period_seconds: None,
        });

        // 资源：models::ResourceLimits → AppResourceRequirements
        let app_resources = request.resources.as_ref().map(|r| AppResourceRequirements {
            cpu: r.cpu.clone(),
            memory: r.memory.clone(),
            storage: r.storage.clone(),
            ephemeral_storage: r.ephemeral_storage.clone(),
        });

        // 宿主机工作空间路径（Docker 模式 bind mount 源；K8s 模式 runtime 用 subPath，忽略此值）
        let host_workspace_path = self.get_host_app_dir(app_id).await.to_string_lossy().to_string();

        let mut builder = ContainerCreateParams::builder()
            .project_id(app_id.to_string())
            .service_type(ServiceType::UserApp)
            .host_workspace_path(host_workspace_path)
            .image_override(request.image.clone())
            .env(request.env.clone().unwrap_or_default())
            .secrets(request.secrets.clone().unwrap_or_default())
            .ports(ports);

        // command 仅在非空时设置（空 vec 会覆盖镜像 CMD）
        if let Some(cmd) = request.command.clone()
            && !cmd.is_empty()
        {
            builder = builder.command(cmd);
        }
        if let Some(hc) = health_check {
            builder = builder.health_check(hc);
        }
        if let Some(ar) = app_resources {
            // 阶段2: storage 落 per-app PVC requests.storage (CSI 服务端 subvolume 配额);
            // ephemeral_storage 仍限 overlay 可写层。
            if let Some(ss) = ar.storage.clone() {
                builder = builder.storage_size(ss);
            }
            builder = builder.app_resources(ar);
        }
        // tenant/space：进 ContainerCreateParams → build_app_labels 打 rcoder.io/tenant、
        // rcoder.io/space label（供对账/过滤）。此前 create 路径漏设，导致 create 出来的
        // 资源缺这两个 label（只有 update 路径 build_container_params_from_update 设了）。
        if let Some(t) = request.tenant_id.clone() {
            builder = builder.tenant_id(t);
        }
        if let Some(s) = request.space_id.clone() {
            builder = builder.space_id(s);
        }

        Ok(builder.build())
    }

    /// UpdateAppRequest → ContainerCreateParams（全量替换语义，image 必填）。
    /// 与 build_container_params 平行；image 缺失 → ERR_VALIDATION
    /// （rcoder 无状态，无法保留旧 image，调用方必须发完整新状态）。
    async fn build_container_params_from_update(
        &self,
        app_id: &str,
        request: &UpdateAppRequest,
    ) -> AppResult<ContainerCreateParams> {
        let image = request.image.clone().ok_or_else(|| {
            AppOperationError::Validation(
                "update requires image (rcoder is stateless, cannot retain previous image)"
                    .to_string(),
            )
        })?;

        let ports: Vec<AppPortSpec> = request
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .map(|p| AppPortSpec {
                        name: p.name.clone(),
                        port: p.port,
                        expose_type: map_expose_type(&p.expose_type),
                        strip_prefix: p.strip_prefix,
                    })
                    .collect()
            })
            .unwrap_or_default();

        if let Some(hc) = &request.health_check
            && matches!(hc.check_type, HealthCheckType::Exec)
        {
            return Err(AppOperationError::Validation(
                "Exec health check is not supported (AppHealthCheck lacks command field); use Http/Tcp instead"
                    .to_string(),
            ));
        }
        let health_check = request.health_check.as_ref().map(|hc| AppHealthCheck {
            check_type: map_health_check_type(&hc.check_type),
            path: hc.path.clone(),
            port: hc.port,
            initial_delay_seconds: None,
            period_seconds: None,
        });
        let app_resources = request.resources.as_ref().map(|r| AppResourceRequirements {
            cpu: r.cpu.clone(),
            memory: r.memory.clone(),
            storage: r.storage.clone(),
            ephemeral_storage: r.ephemeral_storage.clone(),
        });
        let host_workspace_path = self.get_host_app_dir(app_id).await.to_string_lossy().to_string();

        let mut builder = ContainerCreateParams::builder()
            .project_id(app_id.to_string())
            .service_type(ServiceType::UserApp)
            .host_workspace_path(host_workspace_path)
            .image_override(image)
            .env(request.env.clone().unwrap_or_default())
            .secrets(request.secrets.clone().unwrap_or_default())
            .ports(ports);
        if let Some(t) = request.tenant_id.clone() {
            builder = builder.tenant_id(t);
        }
        if let Some(s) = request.space_id.clone() {
            builder = builder.space_id(s);
        }
        if let Some(cmd) = request.command.clone()
            && !cmd.is_empty()
        {
            builder = builder.command(cmd);
        }
        if let Some(hc) = health_check {
            builder = builder.health_check(hc);
        }
        if let Some(ar) = app_resources {
            // 阶段2: storage 落 per-app PVC requests.storage (CSI 服务端 subvolume 配额);
            // ephemeral_storage 仍限 overlay 可写层。
            if let Some(ss) = ar.storage.clone() {
                builder = builder.storage_size(ss);
            }
            builder = builder.app_resources(ar);
        }
        Ok(builder.build())
    }

    /// 实时查询单个应用运行时状态（None 表示不存在）
    async fn fetch_runtime_status(&self, app_id: &str) -> Option<DeploymentStatus> {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(s) => s,
            Err(e) => {
                warn!("[APP] 查询运行时状态失败 app_id={}: {}", app_id, e);
                None
            }
        }
    }

    /// 实时查状态，精确区分两种"查不到"：Ok(None)=集群中真不存在 → "应用不存在"(→404)；
    /// Err=API Server 不可达/RBAC 拒绝 → "查询应用状态失败"(→500)。
    ///
    /// 供需要精确错误分类的读路径（get_app/get_app_stats/ensure_app_exists）使用，
    /// 替代会塌缩错误的 `fetch_runtime_status`（后者仅供 create_app 这类 None 可接受的场景）。
    /// 若误用 fetch_runtime_status，瞬时 API 错误会被当成"应用不存在"→404，触发 Java 误重建。
    async fn fetch_runtime_status_or_err(&self, app_id: &str) -> AppResult<DeploymentStatus> {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(s)) => Ok(s),
            Ok(None) => Err(AppOperationError::NotFound(format!(
                "app does not exist: {app_id}"
            ))),
            Err(e) => {
                warn!("[APP] 查询应用状态失败 app_id={}: {}", app_id, e);
                Err(AppOperationError::Backend(format!(
                    "failed to query app status: {e}"
                )))
            }
        }
    }

    /// 确认 app 存在（集群中有 Deployment/容器），不存在返回"应用不存在"错误。
    /// 调用方（start/stop/restart）据此返回 404，方便 Java 区分并触发 create 重建，
    /// 而非收到 generic 500 误以为系统故障。
    async fn ensure_app_exists(&self, app_id: &str) -> AppResult<()> {
        self.fetch_runtime_status_or_err(app_id).await.map(|_| ())
    }

    /// DeploymentStatus → AppRuntimeInfo（含访问地址构建 + conditions 派生）
    fn build_runtime_info(&self, status: DeploymentStatus) -> AppRuntimeInfo {
        let conditions = derive_conditions(&status);

        // Pingora 模式（不论 Docker/K8s）：runtime status 只含 TCP（HTTP 端口无 binding），
        // 从 pingora_ports 补全 HTTP 端口，保证 get 路径的 ports/access 与 create 一致。
        // Gateway 模式：K8s status.ports 已含 HTTP（HTTPRoute backendRef），无需补。
        // ⚠️ 重启风险（pingora_ports 内存态丢失，已知限制）：
        //   - Docker：HTTP 端口补不出 → access.external.http = null（Java 可感知降级）
        //   - K8s Pingora：status.ports（containerPort）仍含 HTTP → access 返有效 /proxy/{port}，
        //     但 Pingora backend 未重注册 → 访问 404（静默坏路径）。根治：启动从 containerPorts 重建 backends（TODO）
        let ports = if self.config.http_expose == HttpExpose::Pingora {
            let mut merged = status.ports.clone();
            if let Some(http_list) = self.pingora_ports.get(&status.app_id) {
                let http_ports: Vec<u16> = http_list.value().clone();
                // drop Ref guard，避免后续借用 self 时持有 DashMap 读锁
                drop(http_list);
                for hp in http_ports {
                    if !merged.iter().any(|p| p.port == hp) {
                        merged.push(AppPortStatus {
                            name: format!("http-{hp}"),
                            port: hp,
                            expose_type: RtExposeType::Http,
                            external_port: None,
                        });
                    }
                }
            }
            merged
        } else {
            status.ports
        };

        let access = self.build_access_info(&status.app_id, &ports);
        AppRuntimeInfo {
            status: phase_to_status(&status.phase),
            access,
            app_id: status.app_id,
            phase: status.phase,
            message: status.message,
            replicas: status.replicas,
            ready_replicas: status.ready_replicas,
            restart_count: status.restart_count,
            pod_ip: status.pod_ip,
            node: status.node,
            started_at: status.started_at,
            ports,
            conditions,
            resource_version: status.resource_version,
        }
    }

    /// 构建访问信息（按 `http_expose` 决定 HTTP path；一律只返 path，host 由 Java 拼）
    fn build_access_info(&self, app_id: &str, ports: &[AppPortStatus]) -> AccessInfo {
        let http_port = ports.iter().find(|p| p.expose_type == RtExposeType::Http);

        // 一律只返 path，host 由 Java 拼（Java 必然已知 RCoder / gateway 入口，否则访问不了）：
        // - Pingora 模式（默认，两后端统一）：/proxy/{port}
        // - Gateway 模式（K8s 可选）：/apps/{app_id}
        // TCP 初期不对外（external.tcp 空）；internal 始终给 ClusterIP FQDN / 容器名。
        let http_url = match self.config.http_expose {
            HttpExpose::Pingora => http_port.map(|p| format!("/proxy/apps/{}/{}", app_id, p.port)),
            HttpExpose::Gateway => http_port.map(|_| format!("/apps/{}", app_id)),
        };

        // internal domain：K8s = ClusterIP Service FQDN；Docker = 容器名（= 资源名）
        let (domain, short_domain) = match self.config.access_mode {
            AppAccessMode::Docker => {
                let name = format!("{}-{}", ServiceType::UserApp.container_prefix(), app_id);
                (name.clone(), name)
            }
            AppAccessMode::Kubernetes => {
                let cluster_domain = shared_types::get_k8s_cluster_domain();
                let svc = format!("{}-{}-svc", ServiceType::UserApp.container_prefix(), app_id);
                (
                    format!("{}.{}.svc.{}", svc, self.config.namespace, cluster_domain),
                    format!("{}.{}", svc, self.config.namespace),
                )
            }
        };

        AccessInfo {
            external: ExternalAccess {
                http: http_url,
                tcp: vec![], // TCP 初期不对外
            },
            internal: InternalAccess {
                domain,
                short_domain,
                ports: ports
                    .iter()
                    .map(|p| InternalPort {
                        name: p.name.clone(),
                        port: p.port,
                    })
                    .collect(),
            },
        }
    }

    /// 为 HTTP 端口注册 Pingora backend（Pingora 模式，Docker/K8s 统一）。
    /// backend host 按后端：Docker=container_ip，K8s=ClusterIP Service FQDN（Pod 内 kube-dns 解析）。
    /// Gateway 模式不注册（HTTP 走 HTTPRoute）。
    async fn register_pingora_backends(
        &self,
        app_id: &str,
        http_ports: &[u16],
        container_ip: &str,
    ) -> Vec<u16> {
        // Gateway 模式 HTTP 走 HTTPRoute，不经 Pingora——跳过
        if self.config.http_expose == HttpExpose::Gateway {
            return vec![];
        }
        let Some(pingora) = &self.pingora else {
            return vec![];
        };
        // backend host：Docker 用 container_ip；K8s 用 ClusterIP Service FQDN（container_ip 为空）
        let backend_host = match self.config.access_mode {
            AppAccessMode::Docker => {
                if container_ip.is_empty() {
                    warn!(
                        "[APP] Docker 模式 container_ip 为空，跳过 Pingora backend 注册: {}",
                        app_id
                    );
                    return vec![];
                }
                container_ip.to_string()
            }
            AppAccessMode::Kubernetes => {
                let cluster_domain = shared_types::get_k8s_cluster_domain();
                format!(
                    "{}-{}-svc.{}.svc.{}",
                    ServiceType::UserApp.container_prefix(),
                    app_id,
                    self.config.namespace,
                    cluster_domain
                )
            }
        };
        for port in http_ports {
            pingora.add_app_backend(app_id, *port, backend_host.clone());
        }
        if !http_ports.is_empty() {
            self.pingora_ports
                .insert(app_id.to_string(), http_ports.to_vec());
            info!(
                "[APP] Pingora backend 已注册: {} ports={:?} -> {}",
                app_id, http_ports, backend_host
            );
        }
        http_ports.to_vec()
    }

    /// 清理 app 曾注册的 Pingora backend（Pingora 模式）。Gateway 模式未注册过，直接返回。
    async fn unregister_pingora_backends(&self, app_id: &str) {
        if self.config.http_expose == HttpExpose::Gateway {
            return;
        }
        let Some(pingora) = &self.pingora else {
            return;
        };
        if let Some((_, ports)) = self.pingora_ports.remove(app_id) {
            for port in &ports {
                pingora.remove_app_backend(app_id, *port);
            }
            info!("[APP] Pingora backend 已清理: {} ports={:?}", app_id, ports);
        }
    }

    /// 启动时重建 Pingora backends（K8s Pingora 模式，修复重启后 pingora_ports 内存态丢失）。
    /// 从集群列出所有托管 app，按 expose_type（Deployment annotation 还原）重新注册 HTTP 端口的 backend。
    async fn rebuild_pingora_backends(&self) -> AppResult<()> {
        // pingora 未配置（proxy_config 未配）→ 无 backend 可注册；显式说明，避免"0 个 app"被误读为"集群无应用"
        if self.pingora.is_none() {
            info!("[APP] Pingora 未启用（proxy_config 未配），跳过 backends 重建");
            return Ok(());
        }
        let statuses = self
            .runtime
            .list_deployments()
            .await
            .map_err(|e| map_runtime_error("[APP] rebuild list_deployments failed", e))?;
        let mut count = 0;
        for status in &statuses {
            let http_ports: Vec<u16> = status
                .ports
                .iter()
                .filter(|p| p.expose_type == RtExposeType::Http)
                .map(|p| p.port)
                .collect();
            if http_ports.is_empty() {
                continue;
            }
            // register 内部按 access_mode 选 host（K8s=svc FQDN）；container_ip 传空（K8s 不用）
            let registered = self
                .register_pingora_backends(&status.app_id, &http_ports, "")
                .await;
            if !registered.is_empty() {
                count += 1;
            }
        }
        info!(
            "[APP] Pingora backends 重建完成: {count} 个 app（集群共 {} 个托管 app）",
            statuses.len()
        );
        Ok(())
    }

    /// 获取应用目录（rcoder 视角）。
    ///
    /// - K8s per-app: `resolve_workspace_path` 拿 per-app subvolume 聚合路径
    ///   (`{cephfs_root}/{subvolumePath}` = per-app PVC 根); UserApp pod 挂 per-app PVC 根到 /app
    ///   (subPath=None), 故 rcoder 写 PVC 根 (不 join app_id)。
    /// - Docker/无 Ceph: resolve 返回 None → 共享 `workspace_root/{app_id}` (= apps/{app_id},
    ///   运行时适配, 非 per-app 失败)。
    /// - K8s per-app resolve 失败 (Err): **Fail Fast** 返回 Backend 错误, 不 fallback 共享
    ///   (避免 per-app PVC + 共享 PVC 数据面分裂, 见 code-review M1/M2)。
    async fn get_container_app_dir(&self, app_id: &str) -> AppResult<PathBuf> {
        match self
            .runtime
            .resolve_workspace_path(app_id, &ServiceType::UserApp)
            .await
        {
            Ok(Some(base)) => Ok(PathBuf::from(base)), // K8s per-app PVC 根 (不 join app_id)
            Ok(None) => Ok(PathBuf::from(self.config.get_workspace_root()).join(app_id)), // Docker 共享 Local
            Err(e) => Err(AppOperationError::Backend(format!(
                "UserApp per-app PVC resolve 失败 (app_id={app_id}): {e} — 检查 cephfs-root 挂载 + PVC Bound 状态"
            ))),
        }
    }

    /// ensure UserApp per-app 工作空间就绪 (K8s): ensure PVC 带 requests.storage 用户配额 + 重试
    /// resolve 等 ceph-csi provision subvolumePath (SC Immediate 后秒级, 慢可达 10s+)。
    ///
    /// 必须在 create_app_dirs (建目录) + create_deployment (Docker bind mount 需源目录存在) 之前调用:
    /// - K8s: ensure PVC 带配额 + 等 subvolumePath → 后续 create_app_dirs resolve per-app 成功,
    ///   建 code/data/logs 在 per-app PVC 根 (app pod 挂同 PVC, 无分裂); create_deployment 命中
    ///   PVC active 复用 (配额不丢, 因首次 ensure 已带配额)。
    /// - Docker: 无 per-app PVC → no-op (create_app_dirs 走共享 Local, create_deployment bind mount)。
    async fn ensure_app_workspace_ready(
        &self,
        app_id: &str,
        storage_size: Option<&str>,
    ) -> AppResult<()> {
        if !shared_types::is_kubernetes_runtime() {
            return Ok(()); // Docker 无 per-app PVC
        }
        self.runtime
            .ensure_workspace(app_id, &ServiceType::UserApp, storage_size)
            .await
            .map_err(|e| {
                AppOperationError::Backend(format!("ensure UserApp PVC (app_id={app_id}): {e}"))
            })?;
        // 重试 resolve 等 ceph-csi provision subvolumePath 填充 (PVC Bound 后 PV subvolumePath 仍有延迟)
        const MAX_RETRIES: u32 = 15;
        let mut attempt: u32 = 0;
        loop {
            match self
                .runtime
                .resolve_workspace_path(app_id, &ServiceType::UserApp)
                .await
            {
                Ok(Some(_)) | Ok(None) => return Ok(()),
                Err(e) => {
                    attempt += 1;
                    if attempt < MAX_RETRIES {
                        tracing::debug!(
                            "[APP] UserApp PVC subvolumePath pending ({}/{}, app_id={}): {}",
                            attempt,
                            MAX_RETRIES,
                            app_id,
                            e
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    } else {
                        return Err(AppOperationError::Backend(format!(
                            "UserApp PVC subvolumePath 未就绪 (app_id={app_id}, 重试 {MAX_RETRIES} 次): {e}"
                        )));
                    }
                }
            }
        }
    }

    /// 获取应用目录的宿主机路径（Docker bind mount 源）
    ///
    /// Docker 模式：rcoder 通常也运行在容器内，需经 HostPathResolver 将容器内路径
    /// 转为宿主机路径；解析失败回退到原路径。K8s 模式此值不被使用 (subPath)。
    ///
    /// 注意: get_container_app_dir 现返回 Result (K8s per-app 失败 Fail Fast)。本函数保持
    /// PathBuf 签名 (build_container_params 不感知错误), K8s 模式此值本就不用, resolve 失败时
    /// 降级共享路径即可; Docker 模式 resolve Ok(None) → 共享 (正常)。
    async fn get_host_app_dir(&self, app_id: &str) -> PathBuf {
        let p = self
            .get_container_app_dir(app_id)
            .await
            .unwrap_or_else(|_| PathBuf::from(self.config.get_workspace_root()).join(app_id));
        if let Some(resolver) = self.path_resolver.get("default") {
            resolver.resolve_to_host_path(&p).unwrap_or(p)
        } else {
            p
        }
    }

    /// 创建应用工作空间子目录（code/data/logs）。在 ensure_app_workspace_ready (K8s ensure PVC +
    /// 等 subvolumePath) 之后、create_deployment (Docker bind mount 需源目录存在) 之前调用。
    /// Docker: 共享 Local; K8s: per-app PVC 根 (ensure_app_workspace_ready 已确保 resolve 成功)。
    async fn create_app_dirs(&self, app_id: &str) -> AppResult<()> {
        let app_dir = self.get_container_app_dir(app_id).await?;
        fs::create_dir_all(app_dir.join("code"))
            .await
            .map_err(|e| map_io_error("failed to create code dir", e, false))?;
        fs::create_dir_all(app_dir.join("data"))
            .await
            .map_err(|e| map_io_error("failed to create data dir", e, false))?;
        fs::create_dir_all(app_dir.join("logs"))
            .await
            .map_err(|e| map_io_error("failed to create logs dir", e, false))?;
        Ok(())
    }
}

// ============================================================================
// 自由函数辅助
// ============================================================================

/// ContainerRuntimeError → AppOperationError 精确映射。
///
/// `ctx` = 动作前缀（如 "[APP] create_deployment 失败 app_id=app-xxx"），拼入 message。
/// thiserror variant 无 source，`{e}` 即 variant Display（含原始 daemon message）。
fn map_runtime_error(ctx: &str, e: ContainerRuntimeError) -> AppOperationError {
    match e {
        // 容器/deployment 不存在 = app 不存在（404）
        ContainerRuntimeError::ContainerNotFound(_) => {
            AppOperationError::NotFound(format!("{ctx}: {e}"))
        }
        // 其余 8 类（Connection/Creation/Start/Stop/Configuration/Timeout/K8s/Docker）
        // 都是后端运行时/基础设施问题，客户端不可恢复，归 Backend(500)。
        // ConfigurationError 在 runtime 是内部前置条件（params 缺字段），非用户输入，
        // 归 Backend 最保守，避免误判 400。
        _ => AppOperationError::Backend(format!("{ctx}: {e}")),
    }
}

/// io::Error → AppOperationError 精确映射。
///
/// `is_file_op=true`（read_to_string/write/remove_file）：NotFound → FileNotFound(404)
/// `is_file_op=false`（create_dir_all/metadata/canonicalize/read_dir/remove_dir_all）：→ Backend
/// （目录层 NotFound 通常已被上游 app_dir.exists() 守卫拦截，漏到这属异常，归 Backend）
fn map_io_error(ctx: &str, e: std::io::Error, is_file_op: bool) -> AppOperationError {
    match e.kind() {
        std::io::ErrorKind::NotFound if is_file_op => {
            AppOperationError::FileNotFound(format!("{ctx}: {e}"))
        }
        _ => AppOperationError::Backend(format!("{ctx}: {e}")),
    }
}

/// `ArchiveError`（download_utils 解压错误）→ `AppOperationError`。
/// 非法路径 / 解压超限 / 无效压缩包 → `Validation`（400，客户端错误）；
/// IO → `Backend`（500）。
fn map_archive_error(e: ArchiveError) -> AppOperationError {
    match e {
        ArchiveError::PathTraversal(msg) => {
            AppOperationError::Validation(format!("archive contains illegal path: {msg}"))
        }
        ArchiveError::ArchiveBomb { size, max } => AppOperationError::Validation(format!(
            "archive extraction exceeded size limit: {size} > {max}"
        )),
        ArchiveError::InvalidArchive(msg) => {
            AppOperationError::Validation(format!("invalid archive: {msg}"))
        }
        ArchiveError::Io(e) => map_io_error("archive IO error", e, true),
    }
}

/// 校验 upload target（app 根相对路径）。
///
/// 拒绝空 / 绝对路径 / 含 `..` 组件——在 `create_dir_all` **之前**拦截，避免 path traversal
/// 副作用（target 含 `../` 时 create_dir_all 会先在工作空间外创建目录，虽后续 `starts_with`
/// 拒绝，但目录已落盘）。
fn validate_upload_target(target: &str) -> AppResult<()> {
    if target.trim_end_matches('/').is_empty() {
        return Err(AppOperationError::Validation(
            "target must not be empty".to_string(),
        ));
    }
    if target.starts_with('/') {
        return Err(AppOperationError::Validation(
            "target must be relative (app-root-relative)".to_string(),
        ));
    }
    if std::path::Path::new(target)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(AppOperationError::Validation(
            "target must not contain '..'".to_string(),
        ));
    }
    Ok(())
}

/// 校验 app_id 格式（create_app 生成 `app-` + 8 个十六进制字符）
///
/// app_id 直接来自 HTTP 路径参数，会流入文件系统路径拼接（delete/upload/logs/list）。
/// 此校验是路径穿越的纵深防御（Fail Fast）：拒绝 `..`、绝对路径、非法格式，
/// 避免恶意 app_id 触达工作空间目录之外。
fn validate_app_id(app_id: &str) -> AppResult<()> {
    // 必须 app- 前缀（统一，和自动生成一致）
    let rest = app_id.strip_prefix("app-").ok_or_else(|| {
        AppOperationError::Validation("invalid app_id: must start with 'app-'".to_string())
    })?;
    if rest.is_empty() {
        return Err(AppOperationError::Validation(
            "invalid app_id: empty after 'app-'".to_string(),
        ));
    }
    // DNS-1123 label 合规（[a-z0-9]([-a-z0-9]*[a-z0-9])?，≤63；支持 app-order-svc 等业务名）
    if rest.len() > 63
        || !rest
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(AppOperationError::Validation(format!(
            "invalid app_id: must be DNS-1123 label (lowercase alphanumeric or '-', got '{rest}')"
        )));
    }
    if rest.starts_with('-') || rest.ends_with('-') {
        return Err(AppOperationError::Validation(
            "invalid app_id: must not start/end with '-'".to_string(),
        ));
    }
    Ok(())
}

/// 从 PortConfig 列表提取 HTTP 端口号（供 Pingora backend 注册，create/update 共用）
fn http_port_numbers(ports: &Option<Vec<PortConfig>>) -> Vec<u16> {
    ports
        .as_ref()
        .map(|ps| {
            ps.iter()
                .filter(|p| p.expose_type == ExposeType::Http)
                .map(|p| p.port)
                .collect()
        })
        .unwrap_or_default()
}

/// 运行时 phase → 应用状态枚举
fn phase_to_status(phase: &str) -> AppStatus {
    match phase {
        "Running" => AppStatus::Running,
        "Stopped" | "ScaledDown" => AppStatus::Stopped,
        "Starting" | "Pending" | "Creating" => AppStatus::Starting,
        "Error" | "Failed" => AppStatus::Error,
        _ => AppStatus::Created,
    }
}

/// 从 message 中提取简短机器码原因（CrashLoopBackOff / ImagePullBackOff 等）
fn extract_reason(msg: &str) -> Option<&str> {
    const KNOWN: &[&str] = &[
        "CrashLoopBackOff",
        "ImagePullBackOff",
        "ErrImagePull",
        "CreateContainerConfigError",
        "CreateContainerError",
        "InvalidImageName",
        "RunContainerError",
        "StartError",
        "OOMKilled",
    ];
    KNOWN.iter().find(|k| msg.contains(*k)).copied()
}

/// 由 DeploymentStatus 派生 conditions（见设计文档 §6.3 派生表）
///
/// 与 headline 的 `AppStatus` 同源、不矛盾：`status` 给 Java 做状态机判断，
/// `conditions[]` 给人/前端做细粒度诊断。`last_transition_time` 在无状态下不持久
/// 追踪（rcoder 不持有上一时刻状态），统一为 `None`。
fn derive_conditions(status: &DeploymentStatus) -> Vec<Condition> {
    let app_status = phase_to_status(&status.phase);
    let mk = |t: &str, s: &str, reason: Option<&str>, msg: Option<String>| Condition {
        r#type: t.to_string(),
        status: s.to_string(),
        reason: reason.map(str::to_string),
        message: msg,
        last_transition_time: None,
    };
    match app_status {
        AppStatus::Error => {
            let reason = status
                .message
                .as_deref()
                .and_then(extract_reason)
                .unwrap_or("Error");
            vec![
                mk("Error", "True", Some(reason), status.message.clone()),
                mk("Ready", "False", Some("Error"), None),
            ]
        }
        AppStatus::Running => vec![
            mk("Ready", "True", None, None),
            mk("Available", "True", None, None),
        ],
        AppStatus::Stopped => vec![
            mk("Ready", "False", Some("ScaledDown"), None),
            mk("Available", "False", Some("ScaledDown"), None),
        ],
        AppStatus::Starting => vec![
            mk("Progressing", "True", Some("Starting"), None),
            mk("Ready", "False", Some("Starting"), None),
        ],
        AppStatus::Stopping => vec![mk("Progressing", "True", Some("Stopping"), None)],
        AppStatus::Deleting => vec![mk("Progressing", "True", Some("Deleting"), None)],
        AppStatus::Created => vec![mk("Ready", "False", Some("Created"), None)],
    }
}

/// 由 DeploymentStatus 派生健康信息
fn health_from_status(status: &DeploymentStatus) -> HealthInfo {
    HealthInfo {
        status: status.phase.clone(),
        instance: Some(InstanceInfo {
            name: format!(
                "{}-{}",
                ServiceType::UserApp.container_prefix(),
                status.app_id
            ),
            phase: status.phase.clone(),
            ready: status.ready_replicas > 0,
            restart_count: status.restart_count,
            node: status.node.clone().unwrap_or_default(),
            ip: status.pod_ip.clone().unwrap_or_default(),
            started_at: status.started_at.clone(),
        }),
        probes: None,
    }
}

/// models::ExposeType → container_runtime_api::ExposeType
fn map_expose_type(e: &ExposeType) -> RtExposeType {
    match e {
        ExposeType::Http => RtExposeType::Http,
        ExposeType::Tcp => RtExposeType::Tcp,
    }
}

/// models::HealthCheckType → container_runtime_api::HealthCheckType
fn map_health_check_type(t: &HealthCheckType) -> RtHealthCheckType {
    match t {
        HealthCheckType::Http => RtHealthCheckType::Http,
        HealthCheckType::Tcp => RtHealthCheckType::Tcp,
        HealthCheckType::Exec => RtHealthCheckType::Exec,
        HealthCheckType::None => RtHealthCheckType::None,
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

    async fn delete_app_storage(&self, app_id: &str) -> AppResult<()> {
        self.delete_app_storage(app_id).await
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

    async fn restart_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.restart_app(app_id).await
    }

    async fn get_app_logs(&self, app_id: &str, params: LogParams) -> AppResult<Vec<LogEntry>> {
        self.get_app_logs(app_id, params).await
    }

    async fn stream_app_logs(
        &self,
        app_id: &str,
        tail: u32,
    ) -> AppResult<container_runtime_api::mpsc::Receiver<container_runtime_api::ContainerLogEntry>>
    {
        self.stream_app_logs(app_id, tail).await
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

    async fn get_app_file_logs(
        &self,
        app_id: &str,
        file_path: &str,
        tail: u32,
    ) -> AppResult<Vec<LogEntry>> {
        self.get_app_file_logs(app_id, file_path, tail).await
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

    async fn list_files(&self, app_id: &str, subpath: Option<&str>) -> AppResult<Vec<FileInfo>> {
        self.list_files(app_id, subpath).await
    }

    async fn delete_file(&self, app_id: &str, file_path: &str) -> AppResult<()> {
        self.delete_file(app_id, file_path).await
    }
}
