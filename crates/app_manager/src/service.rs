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

use chrono::Utc;
use dashmap::DashMap;
use docker_manager::path::HostPathResolver;
use moka::sync::Cache;
use tracing::{info, instrument, warn};
use uuid::Uuid;

use container_runtime_api::{ExposeType as RtExposeType, HttpExpose, UserAppRuntime};
use rcoder_proxy::PingoraProxyService;

use super::config::{AppAccessMode, AppManagerConfig};
use super::models::*;
use super::utils::*;

/// 应用管理服务（Docker / K8s 统一）
pub struct AppService {
    pub(crate) config: AppManagerConfig,
    /// ISP 收紧 (阶段3): app_manager 只需 workspace (B) + UserApp Deployment (C) 能力,
    /// 不依赖 agent 容器生命周期 (A) —— 类型声明即编译期约束 (调用 agent 方法会编译错).
    pub(crate) runtime: Arc<dyn UserAppRuntime>,
    /// Pingora 代理（Docker 模式用于注册 HTTP backend；K8s 模式通常为 None）
    pub(crate) pingora: Option<Arc<PingoraProxyService>>,
    /// 路径解析器缓存（单例；Docker 模式将 rcoder 容器内路径解析为宿主机路径）
    pub(crate) path_resolver: Cache<String, Arc<HostPathResolver>>,
    /// Docker 模式 Pingora backend 端口登记（app_id → 注册的 HTTP 端口列表）
    ///
    /// 这是**操作副作用的临时缓存**（非业务元数据）：delete 时需要知道曾注册过哪些端口
    /// 才能清理 Pingora backend。rcoder 重启后丢失可接受（Docker 模式定位为开发环境）。
    pub(crate) pingora_ports: DashMap<String, Vec<u16>>,
}

impl AppService {

    /// 创建新的应用管理服务
    pub async fn new(
        config: AppManagerConfig,
        runtime: Arc<dyn UserAppRuntime>,
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
                warn!("[APP] path resolver init failed, using container path: {}", e);
            }
        }

        // K8s 模式：启动校验前置条件（RBAC 等）。失败 log warn 不阻塞（Fail Fast 暴露部署侧
        // RBAC 缺失，而非运行时创建 app 才 403）。Docker 模式 trait 默认 Ok，跳过。
        if config.access_mode == AppAccessMode::Kubernetes {
            match runtime.validate_app_prerequisites().await {
                Ok(_) => info!("[APP] K8s prerequisites validated (RBAC/apps/deployments accessible)"),
                Err(e) => warn!("[APP] K8s prerequisites validation failed, app management may not work: {}", e),
            }
        }

        // 无效组合告警（Fail Fast）：Docker 无 HTTPRoute/gateway 概念，gateway 模式不可用。
        // 不阻塞启动（便于临时切回 pingora），但 HTTP 将不可访问。
        if config.access_mode == AppAccessMode::Docker && config.http_expose == HttpExpose::Gateway
        {
            warn!(
                "[APP] invalid combo access_mode=docker + http_expose=gateway: Docker has no HTTPRoute, gateway mode unavailable, HTTP will be inaccessible; set RCODER_APP_HTTP_EXPOSE=pingora"
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
                "[APP] pingora backends rebuild failed (HTTP temporarily unreachable after restart, recovered on next create/update): {}",
                e
            );
        }
        Ok(svc)
    }


    /// 创建应用
    #[instrument(skip(self, request))]
    pub async fn create_app(&self, request: CreateAppRequest) -> AppResult<AppInfo> {
        let app_id = self.validate_create_request(&request).await?;
        info!(
            "[APP] creating app: {} ({}, mode={:?})",
            request.name, app_id, self.config.access_mode
        );
        self.provision_app_workspace(&app_id, &request).await?;
        self.create_app_runtime(&app_id, &request).await?;
        Ok(self.assemble_app_info(app_id, request).await)
    }

    /// 校验创建请求并解析 app_id（app_id 规范 + 唯一性 + 资源格式 + 端口）。
    /// 任一校验失败 Fail Fast 返回 ERR_VALIDATION / ERR_APP_ALREADY_EXISTS。
    async fn validate_create_request(&self, request: &CreateAppRequest) -> AppResult<String> {
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

        // 资源限制格式（K8s Quantity: storage / ephemeral_storage）→ ERR_VALIDATION
        if let Some(ref resources) = request.resources {
            if let Some(ref s) = resources.storage {
                validate_k8s_storage_size(s).map_err(|e| {
                    AppOperationError::Validation(format!("invalid storage '{}': {}", s, e))
                })?;
            }
            if let Some(ref es) = resources.ephemeral_storage {
                validate_k8s_storage_size(es).map_err(|e| {
                    AppOperationError::Validation(format!(
                        "invalid ephemeral_storage '{}': {}",
                        es, e
                    ))
                })?;
            }
        }

        // 端口校验：HTTP 端口数上限放开（app-runtime 镜像单容器带 pgweb 8081 + ttyd 7681 + 用户应用端口）
        // Pingora path 路由 /proxy/apps/{app_id}/{port}/ 按 (app_id, port) 区分，天然支持多 HTTP 端口
        // gateway 模式（HTTPRoute）仍只支持单 HTTP，在 k8s_deployment 侧单独拦截（这里不拦，让 Pingora 模式可用）
        let http_port_count = request
            .ports
            .as_ref()
            .map(|ps| ps.iter().filter(|p| p.expose_type == ExposeType::Http).count())
            .unwrap_or(0);
        const MAX_HTTP_PORTS: usize = 8;
        if http_port_count > MAX_HTTP_PORTS {
            return Err(AppOperationError::Validation(format!(
                "at most {MAX_HTTP_PORTS} HTTP ports allowed (got {http_port_count})"
            )));
        }
        // 端口号唯一：避免 K8s annotation 解码歧义（同 port 不同 type 会被 HashMap 折叠）
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
        Ok(app_id)
    }

    /// provision：ensure per-app PVC（带用户配额 requests.storage + 等 subvolumePath）+ 建工作空间目录。
    ///
    /// 顺序硬约束：K8s ensure PVC 必须在 create_app_dirs + create_deployment 之前——首次 ensure
    /// 带配额，否则 create_deployment 内 ensure 命中 active 复用会丢配额。Docker 模式 no-op。
    async fn provision_app_workspace(
        &self,
        app_id: &str,
        request: &CreateAppRequest,
    ) -> AppResult<()> {
        let storage_size = request.resources.as_ref().and_then(|r| r.storage.as_deref());
        self.ensure_app_workspace_ready(app_id, storage_size).await?;
        // 创建工作空间目录（code/data/logs）—— Docker: 共享 Local (create_deployment bind mount 源,
        // 必须先存在); K8s: per-app PVC 根 (ensure_app_workspace_ready 已 ensure + 等 subvolumePath)。
        self.create_app_dirs(app_id).await?;
        Ok(())
    }

    /// 创建运行时资源：build params → create_deployment → 注册 Pingora backend。
    ///
    /// 注: UserApp 是新开发逻辑 (application-management-service-v2-design.md), /app 路径
    /// 不涉及历史数据迁移 → 不调 lazy_migrate (新应用无旧数据)。Web/Computer 有历史数据才调。
    async fn create_app_runtime(
        &self,
        app_id: &str,
        request: &CreateAppRequest,
    ) -> AppResult<()> {
        let params = self.build_container_params(app_id, request).await?;
        let container_info = self.runtime.create_deployment(params).await.map_err(|e| {
            map_runtime_error(&format!("[APP] create_deployment failed app_id={app_id}"), e)
        })?;
        info!(
            "[APP] app resources created: {} (container={})",
            app_id, container_info.container_name
        );
        // Docker 模式：为 HTTP 端口注册 Pingora backend（/proxy/apps/{app_id}/{port} → container_ip）
        let http_ports = http_port_numbers(&request.ports);
        self.register_pingora_backends(app_id, &http_ports, &container_info.container_ip)
            .await;
        Ok(())
    }

    /// 装配 AppInfo：实时查运行时状态，合并端口 external_port（K8s node_port），构建 access/health/status。
    ///
    /// status 用运行时 phase 映射（不再硬编码 Running）——刚创建的 Pod 通常还是 Starting，甚至镜像
    /// 拉取失败已 Error；返回真实状态避免"status=Running 但 health=Starting/Error"自相矛盾。
    async fn assemble_app_info(
        &self,
        app_id: String,
        request: CreateAppRequest,
    ) -> AppInfo {
        let runtime_status = self.fetch_runtime_status(&app_id).await;

        // 端口状态：以请求端口为准（expose_type 语义完整），合并运行时返回的 external_port（K8s node_port）。
        // Docker 模式 get_deployment_status 不还原端口语义，Tcp 的 host_port 留空（已知限制）。
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

        let access = self.build_access_info(&app_id, &ports);
        let health = runtime_status
            .as_ref()
            .map(health_from_status)
            .unwrap_or(HealthInfo {
                status: "Unknown".to_string(),
                instance: None,
                probes: None,
            });
        let (status, message) = match &runtime_status {
            Some(s) => (phase_to_status(&s.phase), s.message.clone()),
            None => (AppStatus::Starting, None),
        };

        let now = Utc::now().to_rfc3339();
        AppInfo {
            app_id,
            name: request.name,
            status,
            message,
            image: request.image,
            command: request.command.unwrap_or_default(),
            replicas: 1,
            access,
            health,
            resources: request.resources,
            env: request.env.unwrap_or_default(),
            created_at: now.clone(),
            updated_at: now,
        }
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
                    "[APP] query_apps name/created_at filters require business metadata, rcoder is stateless, ignored"
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
        // 先注销旧 Pingora backend（K8s/Docker 都执行：Docker 旧 container_ip 失效；
        // K8s 下方按本次 http_ports 重新注册到 Service FQDN，注销-重注成对保证一致）。
        self.unregister_pingora_backends(app_id).await;
        let info = self.runtime.patch_deployment(params).await.map_err(|e| {
            map_runtime_error(&format!("[APP] patch_deployment failed app_id={app_id}"), e)
        })?;
        // 重新注册 Pingora backend。http_ports 取本次请求 ports；若未带（K8s 下 update 常
        // 只改 image/env 等部分字段），沿用当前 Deployment 的 HTTP 端口，保证与上面
        // unregister 对称——否则部分更新会丢 Pingora 路由（app 经 /proxy/apps/{id}/{port} 变 502）。
        // 注：register 在 K8s 模式并非 no-op，会把 backend 指到 Service FQDN（与 create 一致）。
        let http_ports = if request.ports.is_some() {
            http_port_numbers(&request.ports)
        } else {
            current
                .ports
                .iter()
                .filter(|p| p.expose_type == RtExposeType::Http)
                .map(|p| p.port)
                .collect::<Vec<u16>>()
        };
        self.register_pingora_backends(app_id, &http_ports, &info.container_ip)
            .await;
        info!("[APP] app updated: {}", app_id);
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
        info!("[APP] deleting app: {} (purge={})", app_id, purge);

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
            // K8s per-agent: app_dir = per-app PVC 根, 清空内容不删根 (同 clear_app_storage)
            if app_dir.exists()
                && let Err(e) = Self::purge_dir_contents(&app_dir).await
            {
                warn!("[APP] purge dir contents failed {:?}: {}", app_dir, e);
            }
            info!("[APP] persistent storage cleared: {}", app_id);
        } else {
            info!(
                "[APP] retained persistent storage (pass purge=true to clear): {}",
                app_id
            );
        }

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

    async fn create_database(
        &self,
        app_id: &str,
        request: CreateDatabaseRequest,
    ) -> AppResult<()> {
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

    async fn restart_app(&self, app_id: &str) -> AppResult<AppRuntimeInfo> {
        self.restart_app(app_id).await
    }

    async fn prepare_release(
        &self,
        app_id: &str,
        request: PrepareReleaseRequest,
    ) -> AppResult<ReleaseInfo> {
        self.prepare_release(app_id, request).await
    }

    async fn activate_release(&self, app_id: &str, release_id: &str) -> AppResult<ReleaseInfo> {
        self.activate_release(app_id, release_id).await
    }

    async fn confirm_release(
        &self,
        app_id: &str,
        release_id: &str,
        healthy: bool,
        message: Option<String>,
    ) -> AppResult<ReleaseInfo> {
        self.confirm_release(app_id, release_id, healthy, message)
            .await
    }

    async fn list_releases(&self, app_id: &str) -> AppResult<ReleaseListResponse> {
        self.list_releases(app_id).await
    }

    async fn delete_release(&self, app_id: &str, release_id: &str) -> AppResult<()> {
        self.delete_release(app_id, release_id).await
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
