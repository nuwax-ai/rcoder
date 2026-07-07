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
        self.register_pingora_backends(&app_id, &request, &container_info.container_ip)
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
                if let Some(ap) = ports.iter_mut().find(|p| p.name == rt_p.name) {
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

        let now = Utc::now().to_rfc3339();
        Ok(AppInfo {
            app_id: app_id.clone(),
            name: request.name.clone(),
            status: AppStatus::Running,
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

    /// 获取应用运行时详情（实时查集群）
    #[instrument(skip(self))]
    pub async fn get_app(&self, app_id: &str) -> Result<AppRuntimeInfo> {
        let status = self
            .fetch_runtime_status(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;
        Ok(self.build_runtime_info(status))
    }

    /// 更新应用配置
    ///
    /// rcoder 无状态：不持久化业务元数据，也无法在缺少旧 spec 的情况下重建 Deployment。
    /// 此接口返回当前运行时状态；如需应用变更（image/env 等），调用方应 delete + create。
    #[instrument(skip(self))]
    pub async fn update_app(
        &self,
        app_id: &str,
        _request: UpdateAppRequest,
    ) -> Result<AppRuntimeInfo> {
        warn!(
            "[APP] update_app: rcoder 无状态，业务元数据由调用方持久化；如需应用变更请 delete + create (app_id={})",
            app_id
        );
        self.get_app(app_id).await
    }

    /// 删除应用
    #[instrument(skip(self))]
    pub async fn delete_app(&self, app_id: &str) -> Result<()> {
        validate_app_id(app_id)?;
        info!("[APP] 删除应用: {}", app_id);

        // 1. Docker 模式：清理 Pingora backend
        self.unregister_pingora_backends(app_id).await;

        // 2. 删除 Deployment 及关联资源（K8s: Service/HTTPRoute/NodePort/ConfigMap/Secret）
        self.runtime
            .delete_deployment(app_id)
            .await
            .map_err(|e| anyhow::anyhow!("删除应用失败: {}", e))?;

        // 3. 清理工作空间目录（共享存储子目录，安全）
        let app_dir = self.get_container_app_dir(app_id);
        if app_dir.exists()
            && let Err(e) = fs::remove_dir_all(&app_dir).await
        {
            warn!("[APP] 清理应用目录失败 {:?}: {}", app_dir, e);
        }

        Ok(())
    }

    /// 启动应用（scale replicas = 1）
    #[instrument(skip(self))]
    pub async fn start_app(&self, app_id: &str) -> Result<AppRuntimeInfo> {
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
        self.runtime
            .restart_deployment(app_id)
            .await
            .map_err(|e| anyhow::anyhow!("重启应用失败: {}", e))?;
        info!("[APP] 应用已重启 (rollout): {}", app_id);
        self.get_app(app_id).await
    }

    /// 获取应用日志（读取共享工作空间 logs/app.log）
    #[instrument(skip(self))]
    pub async fn get_app_logs(&self, app_id: &str, params: LogParams) -> Result<Vec<LogEntry>> {
        validate_app_id(app_id)?;
        let log_file = self
            .get_container_app_dir(app_id)
            .join("logs")
            .join("app.log");
        if !log_file.exists() {
            return Ok(vec![]);
        }
        let content = fs::read_to_string(&log_file)
            .await
            .with_context(|| format!("读取日志失败: {:?}", log_file))?;
        // 日志来源是文件而非 docker stdout 流；时间戳用文件 mtime（最后写入时间）
        let timestamp = match fs::metadata(&log_file).await {
            Ok(m) => m
                .modified()
                .map(|t| {
                    let dt: chrono::DateTime<Utc> = t.into();
                    dt.to_rfc3339()
                })
                .unwrap_or_else(|_| Utc::now().to_rfc3339()),
            Err(_) => Utc::now().to_rfc3339(),
        };
        let tail = params.tail.unwrap_or(1000) as usize;
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(tail);
        Ok(lines[start..]
            .iter()
            .map(|line| LogEntry {
                timestamp: timestamp.clone(),
                stream: "file".to_string(),
                message: line.to_string(),
            })
            .collect())
    }

    /// 获取资源使用情况（best-effort：restart_count 来自运行时；CPU/内存需 metrics-server）
    #[instrument(skip(self))]
    pub async fn get_app_stats(&self, app_id: &str) -> Result<ResourceStats> {
        let status = self
            .fetch_runtime_status(app_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;
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
        let file_path = self.get_container_app_dir(app_id).join("code").join(target);
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&file_path, &file_data).await?;
        Ok(UploadResult {
            file_path: target.to_string(),
            file_size: file_data.len() as u64,
            uploaded_at: Utc::now().to_rfc3339(),
        })
    }

    /// 列出文件
    #[instrument(skip(self))]
    pub async fn list_files(&self, app_id: &str) -> Result<Vec<FileInfo>> {
        validate_app_id(app_id)?;
        let code_dir = self.get_container_app_dir(app_id).join("code");
        if !code_dir.exists() {
            return Ok(vec![]);
        }
        let mut files = Vec::new();
        let mut entries = fs::read_dir(&code_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let metadata = entry.metadata().await?;
            files.push(FileInfo {
                path: entry.file_name().to_string_lossy().to_string(),
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
        let app_dir = self.get_container_app_dir(app_id);
        let full_path = app_dir.join("code").join(file_path);

        // 安全检查：确保路径在应用 code 目录内
        let canonical_path = full_path.canonicalize()?;
        let code_dir = app_dir.join("code").canonicalize()?;
        if !canonical_path.starts_with(&code_dir) {
            return Err(anyhow::anyhow!("路径不在应用目录内"));
        }
        if !canonical_path.exists() {
            return Err(anyhow::anyhow!("文件不存在: {}", file_path));
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

    /// DeploymentStatus → AppRuntimeInfo（含访问地址构建）
    fn build_runtime_info(&self, status: DeploymentStatus) -> AppRuntimeInfo {
        let access = self.build_access_info(&status.app_id, &status.ports);
        AppRuntimeInfo {
            status: phase_to_status(&status.phase),
            access,
            app_id: status.app_id,
            phase: status.phase,
            replicas: status.replicas,
            ready_replicas: status.ready_replicas,
            restart_count: status.restart_count,
            pod_ip: status.pod_ip,
            node: status.node,
            started_at: status.started_at,
            ports: status.ports,
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
        request: &CreateAppRequest,
        container_ip: &str,
    ) -> Vec<u16> {
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
        let http_ports: Vec<u16> = request
            .ports
            .as_ref()
            .map(|ps| {
                ps.iter()
                    .filter(|p| p.expose_type == ExposeType::Http)
                    .map(|p| p.port)
                    .collect()
            })
            .unwrap_or_default();

        for port in &http_ports {
            pingora.add_backend(*port, container_ip.to_string()).await;
        }
        if !http_ports.is_empty() {
            self.pingora_ports
                .insert(app_id.to_string(), http_ports.clone());
            info!(
                "[APP] Pingora backend 已注册: {} ports={:?} -> {}",
                app_id, http_ports, container_ip
            );
        }
        http_ports
    }

    /// Docker 模式：清理 app 曾注册的 Pingora backend
    async fn unregister_pingora_backends(&self, app_id: &str) {
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

    async fn delete_app(&self, app_id: &str) -> Result<()> {
        self.delete_app(app_id).await
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

    async fn list_files(&self, app_id: &str) -> Result<Vec<FileInfo>> {
        self.list_files(app_id).await
    }

    async fn delete_file(&self, app_id: &str, file_path: &str) -> Result<()> {
        self.delete_file(app_id, file_path).await
    }
}
