//! 应用管理服务层（统一 Docker / K8s 后端，无状态）
//!
//! rcoder 是无状态的应用 pod 引擎：
//! - 写操作（create/start/stop/restart/delete）转调 [`ContainerRuntime`] 的 Deployment 能力；
//! - 读操作（get/query/list）实时查集群，返回 [`AppRuntimeInfo`]；
//! - 业务元数据（name/image/command/env 等）由调用方（Java）持久化，rcoder 不存。
//!
//! K8s 模式下 `create_deployment` 一并创建 ConfigMap/Secret/Service/HTTPRoute/NodePort；
//! Docker 模式下 `create_deployment` 创建容器并加入主网络，本服务额外为 HTTP 端口注册
//! Pingora backend（`/proxy/{port}` → container_ip），TCP 端口由 runtime 做 port_bindings。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use dashmap::DashMap;
use docker_manager::path::HostPathResolver;
use moka::sync::Cache;
use tokio::fs;
use tracing::{info, instrument, warn};
use uuid::Uuid;

use container_runtime_api::{
    AppHealthCheck, AppPortSpec, AppResourceRequirements, ContainerCreateParams, ContainerRuntime,
    DeploymentStatus, ExposeType as RtExposeType, HealthCheckType as RtHealthCheckType,
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
    ) -> Result<Self> {
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

        Ok(Self {
            config,
            runtime,
            pingora,
            path_resolver,
            pingora_ports: DashMap::new(),
        })
    }

    /// 创建应用
    #[instrument(skip(self, request))]
    pub async fn create_app(&self, request: CreateAppRequest) -> Result<AppInfo> {
        let app_id = format!("app-{}", &Uuid::new_v4().to_string()[..8]);
        info!(
            "[APP] 创建应用: {} ({}, mode={:?})",
            request.name, app_id, self.config.access_mode
        );

        // 0. 校验资源限制格式（K8s Quantity: storage / ephemeral_storage）
        if let Some(ref resources) = request.resources {
            if let Some(ref s) = resources.storage {
                crate::handler::pod_handler::validate_k8s_storage_size(s)
                    .map_err(|e| anyhow::anyhow!("invalid storage '{}': {}", s, e))?;
            }
            if let Some(ref es) = resources.ephemeral_storage {
                crate::handler::pod_handler::validate_k8s_storage_size(es)
                    .map_err(|e| anyhow::anyhow!("invalid ephemeral_storage '{}': {}", es, e))?;
            }
        }

        // 1. 创建应用工作空间目录（code/data/logs）
        self.create_app_dirs(&app_id).await?;

        // 2. 构建容器创建参数（UserApp）
        let params = self.build_container_params(&app_id, &request)?;

        // 3. 创建 Deployment / 容器（K8s 含 ConfigMap/Secret/Service/HTTPRoute/NodePort）
        let container_info = self
            .runtime
            .create_deployment(params)
            .await
            .map_err(|e| anyhow::anyhow!("创建应用失败: {}", e))
            .context(format!("[APP] create_deployment 失败 app_id={app_id}"))?;
        info!(
            "[APP] 应用资源创建成功: {} (container={})",
            app_id, container_info.container_name
        );

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
    pub async fn list_app_runtimes(&self) -> Result<Vec<AppRuntimeInfo>> {
        let statuses = self
            .runtime
            .list_deployments()
            .await
            .map_err(|e| anyhow::anyhow!("列出应用失败: {}", e))?;
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
    ) -> Result<PaginatedResponse<AppRuntimeInfo>> {
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
    pub async fn get_app(&self, app_id: &str) -> Result<AppRuntimeInfo> {
        let status = self.fetch_runtime_status_or_err(app_id).await?;
        Ok(self.build_runtime_info(status))
    }

    /// 更新应用配置
    ///
    /// **当前不支持 in-place 更新**（Fail Fast 拒绝，而非静默返回当前状态假装成功）：
    /// rcoder 无状态不持久化业务元数据，无法在缺少旧 spec 时做安全 patch；
    /// `UpdateAppRequest` 也不含 ports/health_check，无法 delete+create 完整重建。
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
    ) -> Result<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        let params = self.build_container_params_from_update(app_id, &request)?;
        // Docker 模式：旧容器 IP 即将失效，先注销 Pingora backend（K8s no-op）
        self.unregister_pingora_backends(app_id).await;
        let info = self
            .runtime
            .patch_deployment(params)
            .await
            .map_err(|e| anyhow::anyhow!("更新应用失败: {}", e))
            .with_context(|| format!("[APP] patch_deployment 失败 app_id={app_id}"))?;
        // Docker 模式：新容器 IP，重新注册 Pingora backend（K8s no-op）
        let http_ports = http_port_numbers(&request.ports);
        self.register_pingora_backends(app_id, &http_ports, &info.container_ip)
            .await;
        info!("[APP] 应用已更新: {}", app_id);
        self.get_app(app_id).await
    }

    /// 删除应用（v2 §5.3：默认保留持久存储，purge=true 才清空数据面）。
    #[instrument(skip(self))]
    pub async fn delete_app(&self, app_id: &str, purge: bool) -> Result<()> {
        validate_app_id(app_id)?;
        info!("[APP] 删除应用: {} (purge={})", app_id, purge);

        // 1. Docker 模式：清理 Pingora backend
        self.unregister_pingora_backends(app_id).await;

        // 2. 删除计算资源（K8s: Deployment/Service/HTTPRoute/NodePort/ConfigMap/Secret
        //    + label orphan 扫描兜底；Docker: 容器）。持久存储默认保留。
        self.runtime
            .delete_deployment(app_id)
            .await
            .map_err(|e| anyhow::anyhow!("删除应用失败: {}", e))?;

        // 3. 仅 purge=true 时清空持久存储（code/data/logs 目录）。
        //    默认保留：应用可重建，数据不可再生（v2 §5.3 数据安全）。
        if purge {
            let app_dir = self.get_container_app_dir(app_id);
            if app_dir.exists()
                && let Err(e) = fs::remove_dir_all(&app_dir).await
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
    pub async fn get_app_storage(&self, app_id: &str) -> Result<StorageInfo> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id);
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
    pub async fn delete_app_storage(&self, app_id: &str) -> Result<()> {
        validate_app_id(app_id)?;
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(_)) => {
                return Err(AppOperationError::invalid_state(format!(
                    "应用 {app_id} 仍存在，请先 delete 再清空存储（避免损坏在用数据）"
                ))
                .into());
            }
            Ok(None) => {}
            Err(e) => {
                warn!("[APP] 查询应用状态失败 app_id={}: {}", app_id, e);
                return Err(AppOperationError::backend(format!("查询应用状态失败: {e}")).into());
            }
        }
        let app_dir = self.get_container_app_dir(app_id);
        if app_dir.exists()
            && let Err(e) = fs::remove_dir_all(&app_dir).await
        {
            return Err(anyhow::anyhow!("清空存储失败: {}", e));
        }
        info!("[APP] 已清空应用存储: {}", app_id);
        Ok(())
    }

    /// 分页查询持久存储（强制分页，无全量模式）。
    /// 过滤：orphan_only、app_ids 生效；tenant_id/space_id 在无状态下不支持（rcoder 不持
    /// app→租户映射），提供则 warn 忽略。
    pub async fn query_storage(
        &self,
        request: QueryStorageRequest,
    ) -> Result<PaginatedResponse<StorageInfo>> {
        if request.page == 0 {
            return Err(
                AppOperationError::new(shared_types::ERR_VALIDATION, "page 从 1 开始").into(),
            );
        }
        if request.page_size == 0 || request.page_size > 100 {
            return Err(AppOperationError::new(
                shared_types::ERR_VALIDATION,
                "page_size 须在 1..=100",
            )
            .into());
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
            .map_err(|e| anyhow::anyhow!("列出应用失败: {}", e))?
            .into_iter()
            .map(|s| s.app_id)
            .collect();
        // 单层列 workspace_root（不递归）
        let workspace_root = self.config.get_workspace_root();
        let mut entries: Vec<String> = match tokio::fs::read_dir(workspace_root).await {
            Ok(mut rd) => {
                let mut v = vec![];
                while let Ok(Some(de)) = rd.next_entry().await {
                    if de.file_type().await.map(|t| t.is_dir()).unwrap_or(false)
                        && let Some(name) = de.file_name().to_str().map(|s| s.to_string())
                    {
                        v.push(name);
                    }
                }
                v
            }
            Err(_) => vec![],
        };
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
            let app_dir = self.get_container_app_dir(&app_id);
            let is_orphan = !existing.contains(&app_id);
            let metadata = tokio::fs::metadata(&app_dir).await.ok();
            let modified_at = metadata
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
            items.push(StorageInfo {
                app_id,
                exists: metadata.is_some(),
                path: app_dir.to_string_lossy().to_string(),
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
        matches!(self.runtime.get_deployment_status(app_id).await, Ok(None))
    }

    /// 启动应用（scale replicas = 1）
    #[instrument(skip(self))]
    pub async fn start_app(&self, app_id: &str) -> Result<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime
            .scale_deployment(app_id, 1)
            .await
            .map_err(|e| anyhow::anyhow!("启动应用失败: {}", e))?;
        info!("[APP] 应用已启动 (scale=1): {}", app_id);
        self.get_app(app_id).await
    }

    /// 停止应用（scale replicas = 0）
    #[instrument(skip(self))]
    pub async fn stop_app(&self, app_id: &str) -> Result<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime
            .scale_deployment(app_id, 0)
            .await
            .map_err(|e| anyhow::anyhow!("停止应用失败: {}", e))?;
        info!("[APP] 应用已停止 (scale=0): {}", app_id);
        self.get_app(app_id).await
    }

    /// 重启应用（rollout restart）
    #[instrument(skip(self))]
    pub async fn restart_app(&self, app_id: &str) -> Result<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        self.ensure_app_exists(app_id).await?;
        self.runtime
            .restart_deployment(app_id)
            .await
            .map_err(|e| anyhow::anyhow!("重启应用失败: {}", e))?;
        info!("[APP] 应用已重启 (rollout): {}", app_id);
        self.get_app(app_id).await
    }

    /// 获取应用日志（实时拉容器 stdout/stderr：K8s Pod logs / docker logs）。
    ///
    /// `follow` 流式当前未实现（runtime 返回 tail 快照），`since` 暂未透传；
    /// SSE/WebSocket 实时流留待后续增强。
    #[instrument(skip(self))]
    pub async fn get_app_logs(&self, app_id: &str, params: LogParams) -> Result<Vec<LogEntry>> {
        validate_app_id(app_id)?;
        let tail = params.tail.unwrap_or(1000);
        let timestamps = params.timestamps.unwrap_or(true);
        let entries = self
            .runtime
            .get_app_logs(app_id, tail, timestamps)
            .await
            .map_err(|e| anyhow::anyhow!("读取日志失败: {}", e))
            .with_context(|| format!("[APP] get_app_logs 失败 app_id={app_id}"))?;
        Ok(entries
            .into_iter()
            .map(|e| LogEntry {
                timestamp: e.timestamp.unwrap_or_default(),
                stream: e.stream,
                message: e.message,
            })
            .collect())
    }

    /// 获取资源使用情况（best-effort：restart_count 来自运行时；CPU/内存需 metrics-server）
    #[instrument(skip(self))]
    pub async fn get_app_stats(&self, app_id: &str) -> Result<ResourceStats> {
        let status = self.fetch_runtime_status_or_err(app_id).await?;
        Ok(ResourceStats {
            restart_count: status.restart_count,
            ..Default::default()
        })
    }

    /// 获取应用事件（best-effort：当前返回空，TODO 接 K8s events）
    #[instrument(skip(self))]
    pub async fn get_app_events(&self, app_id: &str) -> Result<Vec<String>> {
        let _ = app_id;
        Ok(vec![])
    }

    /// 上传文件
    #[instrument(skip(self, file_data))]
    pub async fn upload_file(
        &self,
        app_id: &str,
        file_data: Vec<u8>,
        target: &str,
    ) -> Result<UploadResult> {
        validate_app_id(app_id)?;
        // target 相对 app 根目录（设计 §8.4：默认 "code/{name}"，也可写 data/ logs/）。
        let app_dir = self.get_container_app_dir(app_id);
        // 先确保 app 根存在，便于 canonicalize 取真实基准路径
        fs::create_dir_all(&app_dir).await?;
        let file_path = app_dir.join(target);
        // 防穿越：canonicalize 父目录（已存在）后校验仍在 app 目录内，与 delete_file 对称。
        // 拦截两类攻击：绝对路径 target（PathBuf::join 遇绝对路径会替换基准）与 ../../ 上跳。
        let canonical_app_dir = app_dir.canonicalize()?;
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).await?;
            let canonical_parent = parent.canonicalize()?;
            if !canonical_parent.starts_with(&canonical_app_dir) {
                return Err(anyhow::anyhow!("路径不在应用目录内"));
            }
        }
        fs::write(&file_path, &file_data).await?;
        Ok(UploadResult {
            file_path: target.to_string(),
            file_size: file_data.len() as u64,
            uploaded_at: Utc::now().to_rfc3339(),
        })
    }

    /// 列出文件（app 根目录，或其子目录如 "code"/"data"/"logs"）。
    ///
    /// `subpath` 为 None/空 → 列 app 根；否则列 `app_dir/{subpath}`。返回的 `path` 字段是
    /// **app-root-relative**（如 "code/app.jar"），可直接作为 upload 的 target / delete 的 path，
    /// 与这两个接口的约定一致。防穿越：子目录 canonicalize 后必须仍在 app 目录内。
    #[instrument(skip(self))]
    pub async fn list_files(&self, app_id: &str, subpath: Option<&str>) -> Result<Vec<FileInfo>> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id);
        if !app_dir.exists() {
            return Ok(vec![]);
        }
        let canonical_app_dir = app_dir.canonicalize()?;
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
                let canonical_full = full.canonicalize()?;
                if !canonical_full.starts_with(&canonical_app_dir) {
                    return Err(anyhow::anyhow!("路径不在应用目录内"));
                }
                canonical_full
            }
            None => canonical_app_dir,
        };
        // 返回 app-root-relative 路径（sub 存在时前缀 "sub/"）
        let rel_prefix = sub.map(|p| format!("{p}/")).unwrap_or_default();
        let mut files = Vec::new();
        let mut entries = fs::read_dir(&target_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let metadata = entry.metadata().await?;
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
    pub async fn delete_file(&self, app_id: &str, file_path: &str) -> Result<()> {
        validate_app_id(app_id)?;
        // file_path 相对 app 根目录（与 upload_file 的 target 同约定：可指向 code/ data/ logs/）
        let app_dir = self.get_container_app_dir(app_id);
        if !app_dir.exists() {
            return Err(anyhow::anyhow!("应用目录不存在"));
        }
        let full_path = app_dir.join(file_path);
        // 先 exists 守卫，避免 canonicalize 对不存在路径抛 OS 错误（导致 500 而非 404）
        if !full_path.exists() {
            return Err(anyhow::anyhow!("文件不存在: {}", file_path));
        }

        // 安全检查：canonicalize 后确保路径仍在 app 目录内（防 ../ 穿越到外部）
        let canonical_path = full_path.canonicalize()?;
        let canonical_app_dir = app_dir.canonicalize()?;
        if !canonical_path.starts_with(&canonical_app_dir) {
            return Err(anyhow::anyhow!("路径不在应用目录内"));
        }

        if canonical_path.is_dir() {
            fs::remove_dir_all(&canonical_path).await?;
        } else {
            fs::remove_file(&canonical_path).await?;
        }
        info!("[APP] 文件已删除: {}", file_path);
        Ok(())
    }

    // ========================================================================
    // 辅助方法
    // ========================================================================

    /// 构建 ContainerCreateParams（UserApp）
    fn build_container_params(
        &self,
        app_id: &str,
        request: &CreateAppRequest,
    ) -> Result<ContainerCreateParams> {
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
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Exec 健康检查当前未支持（AppHealthCheck 无 command 字段），Fail Fast 拒绝，
        // 避免静默丢弃用户配置（K8s build_probe 对 Exec 返回 None → 容器被视为永远健康）
        if let Some(hc) = &request.health_check
            && matches!(hc.check_type, HealthCheckType::Exec)
        {
            anyhow::bail!(
                "Exec 健康检查暂不支持（AppHealthCheck 缺少 command 字段），请改用 Http/Tcp"
            );
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
        let host_workspace_path = self.get_host_app_dir(app_id).to_string_lossy().to_string();

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
            builder = builder.app_resources(ar);
        }

        Ok(builder.build())
    }

    /// UpdateAppRequest → ContainerCreateParams（全量替换语义，image 必填）。
    /// 与 build_container_params 平行；image 缺失 → ERR_VALIDATION
    /// （rcoder 无状态，无法保留旧 image，调用方必须发完整新状态）。
    fn build_container_params_from_update(
        &self,
        app_id: &str,
        request: &UpdateAppRequest,
    ) -> Result<ContainerCreateParams> {
        let image = request.image.clone().ok_or_else(|| {
            AppOperationError::new(
                shared_types::ERR_VALIDATION,
                "update 需要 image（rcoder 无状态，无法保留旧 image）",
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
                    })
                    .collect()
            })
            .unwrap_or_default();

        if let Some(hc) = &request.health_check
            && matches!(hc.check_type, HealthCheckType::Exec)
        {
            anyhow::bail!(
                "Exec 健康检查暂不支持（AppHealthCheck 缺少 command 字段），请改用 Http/Tcp"
            );
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
        let host_workspace_path = self.get_host_app_dir(app_id).to_string_lossy().to_string();

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
    async fn fetch_runtime_status_or_err(&self, app_id: &str) -> Result<DeploymentStatus> {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(s)) => Ok(s),
            Ok(None) => Err(anyhow::anyhow!("应用不存在: {}", app_id)),
            Err(e) => {
                warn!("[APP] 查询应用状态失败 app_id={}: {}", app_id, e);
                Err(anyhow::anyhow!("查询应用状态失败: {}", e))
            }
        }
    }

    /// 确认 app 存在（集群中有 Deployment/容器），不存在返回"应用不存在"错误。
    /// 调用方（start/stop/restart）据此返回 404，方便 Java 区分并触发 create 重建，
    /// 而非收到 generic 500 误以为系统故障。
    async fn ensure_app_exists(&self, app_id: &str) -> Result<()> {
        self.fetch_runtime_status_or_err(app_id).await.map(|_| ())
    }

    /// DeploymentStatus → AppRuntimeInfo（含访问地址构建 + conditions 派生）
    fn build_runtime_info(&self, status: DeploymentStatus) -> AppRuntimeInfo {
        let conditions = derive_conditions(&status);
        let access = self.build_access_info(&status.app_id, &status.ports);
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
            ports: status.ports,
            conditions,
        }
    }

    /// 构建访问信息（按 access_mode 分支）
    fn build_access_info(&self, app_id: &str, ports: &[AppPortStatus]) -> AccessInfo {
        let http_port = ports.iter().find(|p| p.expose_type == RtExposeType::Http);

        // K8s 模式：rcoder 在 Pod 内不知 Gateway 的外部入口（LB/NodePort），故只返回
        // HTTPRoute path（/apps/{app_id}）+ TCP 的 NodePort 数字，外部完整 URL 由调用方
        // （Java）用自己知道的入口拼。Docker 模式：Pingora /proxy + external_host（开发环境）。
        let (http_url, tcp_ports, domain, short_domain) = match self.config.access_mode {
            AppAccessMode::Docker => {
                let http = http_port.map(|p| {
                    format!(
                        "http://{}:{}/proxy/{}",
                        self.config.get_external_host(),
                        self.config.get_pingora_listen_port(),
                        p.port
                    )
                });
                let tcp_ports: Vec<TcpPortMapping> = ports
                    .iter()
                    .filter(|p| p.expose_type == RtExposeType::Tcp)
                    .map(|p| {
                        let ext = p.external_port.unwrap_or(0);
                        TcpPortMapping {
                            name: p.name.clone(),
                            node_port: ext,
                            access_url: format!(
                                "tcp://{}:{}",
                                self.config.get_external_host(),
                                ext
                            ),
                        }
                    })
                    .collect();
                let name = format!("{}-{}", ServiceType::UserApp.container_prefix(), app_id);
                (http, tcp_ports, name.clone(), name)
            }
            AppAccessMode::Kubernetes => {
                let http = http_port.map(|_| format!("/apps/{}", app_id));
                let tcp_ports: Vec<TcpPortMapping> = ports
                    .iter()
                    .filter(|p| p.expose_type == RtExposeType::Tcp)
                    .map(|p| {
                        let ext = p.external_port.unwrap_or(0);
                        TcpPortMapping {
                            name: p.name.clone(),
                            node_port: ext,
                            // host 由调用方拼（rcoder 不知外部入口）；node_port 为真实 NodePort
                            access_url: format!("tcp://<gateway>:{ext}"),
                        }
                    })
                    .collect();
                let cluster_domain = shared_types::get_k8s_cluster_domain();
                let svc = format!("{}-{}-svc", ServiceType::UserApp.container_prefix(), app_id);
                let fqdn = format!("{}.{}.svc.{}", svc, self.config.namespace, cluster_domain);
                (
                    http,
                    tcp_ports,
                    fqdn,
                    format!("{}.{}", svc, self.config.namespace),
                )
            }
        };

        AccessInfo {
            external: ExternalAccess {
                http: http_url,
                tcp: tcp_ports,
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

    /// Docker 模式：为 HTTP 端口注册 Pingora backend，返回注册的端口列表
    async fn register_pingora_backends(
        &self,
        app_id: &str,
        http_ports: &[u16],
        container_ip: &str,
    ) -> Vec<u16> {
        // K8s 模式 HTTP 走 Gateway（HTTPRoute），不经 Pingora /proxy——跳过注册
        if self.config.access_mode != AppAccessMode::Docker {
            return vec![];
        }
        let Some(pingora) = &self.pingora else {
            return vec![];
        };
        if container_ip.is_empty() {
            warn!(
                "[APP] Docker 模式 container_ip 为空，跳过 Pingora backend 注册: {}",
                app_id
            );
            return vec![];
        }
        for port in http_ports {
            pingora.add_backend(*port, container_ip.to_string()).await;
        }
        if !http_ports.is_empty() {
            self.pingora_ports
                .insert(app_id.to_string(), http_ports.to_vec());
            info!(
                "[APP] Pingora backend 已注册: {} ports={:?} -> {}",
                app_id, http_ports, container_ip
            );
        }
        http_ports.to_vec()
    }

    /// Docker 模式：清理 app 曾注册的 Pingora backend
    async fn unregister_pingora_backends(&self, app_id: &str) {
        // K8s 模式未注册过 Pingora backend，跳过
        if self.config.access_mode != AppAccessMode::Docker {
            return;
        }
        let Some(pingora) = &self.pingora else {
            return;
        };
        if let Some((_, ports)) = self.pingora_ports.remove(app_id) {
            for port in &ports {
                pingora.remove_backend(*port).await;
            }
            info!("[APP] Pingora backend 已清理: {} ports={:?}", app_id, ports);
        }
    }

    /// 获取应用目录（rcoder 视角：workspace_root/app_id）
    fn get_container_app_dir(&self, app_id: &str) -> PathBuf {
        PathBuf::from(self.config.get_workspace_root()).join(app_id)
    }

    /// 获取应用目录的宿主机路径（Docker bind mount 源）
    ///
    /// Docker 模式：rcoder 通常也运行在容器内，需经 HostPathResolver 将容器内路径
    /// 转为宿主机路径；解析失败回退到原路径。K8s 模式此值不被使用。
    fn get_host_app_dir(&self, app_id: &str) -> PathBuf {
        let p = self.get_container_app_dir(app_id);
        if let Some(resolver) = self.path_resolver.get("default") {
            resolver.resolve_to_host_path(&p).unwrap_or(p)
        } else {
            p
        }
    }

    /// 创建应用工作空间子目录
    async fn create_app_dirs(&self, app_id: &str) -> Result<()> {
        let app_dir = self.get_container_app_dir(app_id);
        fs::create_dir_all(app_dir.join("code")).await?;
        fs::create_dir_all(app_dir.join("data")).await?;
        fs::create_dir_all(app_dir.join("logs")).await?;
        Ok(())
    }
}

// ============================================================================
// 自由函数辅助
// ============================================================================

/// 校验 app_id 格式（create_app 生成 `app-` + 8 个十六进制字符）
///
/// app_id 直接来自 HTTP 路径参数，会流入文件系统路径拼接（delete/upload/logs/list）。
/// 此校验是路径穿越的纵深防御（Fail Fast）：拒绝 `..`、绝对路径、非法格式，
/// 避免恶意 app_id 触达工作空间目录之外。
fn validate_app_id(app_id: &str) -> Result<()> {
    let rest = app_id
        .strip_prefix("app-")
        .ok_or_else(|| anyhow::anyhow!("invalid app_id: {app_id} (expected prefix 'app-')"))?;
    if rest.len() == 8 && rest.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "invalid app_id: {app_id} (expected 'app-' + 8 hex chars)"
        ))
    }
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
    async fn create_app(&self, request: CreateAppRequest) -> Result<AppInfo> {
        self.create_app(request).await
    }

    async fn query_apps(
        &self,
        request: QueryAppsRequest,
    ) -> Result<PaginatedResponse<AppRuntimeInfo>> {
        self.query_apps(request).await
    }

    async fn list_app_runtimes(&self) -> Result<Vec<AppRuntimeInfo>> {
        self.list_app_runtimes().await
    }

    async fn get_app(&self, app_id: &str) -> Result<AppRuntimeInfo> {
        self.get_app(app_id).await
    }

    async fn update_app(&self, app_id: &str, request: UpdateAppRequest) -> Result<AppRuntimeInfo> {
        self.update_app(app_id, request).await
    }

    async fn delete_app(&self, app_id: &str, purge: bool) -> Result<()> {
        self.delete_app(app_id, purge).await
    }

    async fn get_app_storage(&self, app_id: &str) -> Result<StorageInfo> {
        self.get_app_storage(app_id).await
    }

    async fn delete_app_storage(&self, app_id: &str) -> Result<()> {
        self.delete_app_storage(app_id).await
    }

    async fn query_storage(
        &self,
        request: QueryStorageRequest,
    ) -> Result<PaginatedResponse<StorageInfo>> {
        self.query_storage(request).await
    }

    async fn start_app(&self, app_id: &str) -> Result<AppRuntimeInfo> {
        self.start_app(app_id).await
    }

    async fn stop_app(&self, app_id: &str) -> Result<AppRuntimeInfo> {
        self.stop_app(app_id).await
    }

    async fn restart_app(&self, app_id: &str) -> Result<AppRuntimeInfo> {
        self.restart_app(app_id).await
    }

    async fn get_app_logs(&self, app_id: &str, params: LogParams) -> Result<Vec<LogEntry>> {
        self.get_app_logs(app_id, params).await
    }

    async fn get_app_stats(&self, app_id: &str) -> Result<ResourceStats> {
        self.get_app_stats(app_id).await
    }

    async fn get_app_events(&self, app_id: &str) -> Result<Vec<String>> {
        self.get_app_events(app_id).await
    }

    async fn upload_file(
        &self,
        app_id: &str,
        file_data: Vec<u8>,
        target: &str,
    ) -> Result<UploadResult> {
        self.upload_file(app_id, file_data, target).await
    }

    async fn list_files(&self, app_id: &str, subpath: Option<&str>) -> Result<Vec<FileInfo>> {
        self.list_files(app_id, subpath).await
    }

    async fn delete_file(&self, app_id: &str, file_path: &str) -> Result<()> {
        self.delete_file(app_id, file_path).await
    }
}
