//! 应用管理服务层

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use bollard::Docker;
use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use bollard::query_parameters::{
    CreateContainerOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use chrono::Utc;
use dashmap::DashMap;
use docker_manager::path::HostPathResolver;
use futures::StreamExt;
use moka::sync::Cache;
use tokio::fs;
use tracing::{error, info, instrument, warn};
use uuid::Uuid;

use super::config::AppManagerConfig;
use super::models::*;

/// 容器名称前缀
const CONTAINER_PREFIX: &str = "rcoder-app";

/// 工作空间根目录（容器内路径）
const WORKSPACE_ROOT: &str = "/app/app-workspace";

/// 计算 CPU 使用率
fn calculate_cpu_percent(stats: &bollard::models::ContainerStatsResponse) -> f64 {
    let cpu_stats = match &stats.cpu_stats {
        Some(s) => s,
        None => return 0.0,
    };
    let precpu_stats = match &stats.precpu_stats {
        Some(s) => s,
        None => return 0.0,
    };

    let cpu_usage = cpu_stats
        .cpu_usage
        .as_ref()
        .and_then(|u| u.total_usage)
        .unwrap_or(0);
    let precpu_usage = precpu_stats
        .cpu_usage
        .as_ref()
        .and_then(|u| u.total_usage)
        .unwrap_or(0);

    let cpu_delta = cpu_usage as f64 - precpu_usage as f64;
    let system_cpu_delta = cpu_stats.system_cpu_usage.unwrap_or(0) as f64
        - precpu_stats.system_cpu_usage.unwrap_or(0) as f64;
    let number_cpus = cpu_stats.online_cpus.unwrap_or(1) as f64;

    if system_cpu_delta > 0.0 && cpu_delta >= 0.0 {
        (cpu_delta / system_cpu_delta) * number_cpus * 100.0
    } else {
        0.0
    }
}

/// 计算网络使用
fn calculate_network_usage(stats: &bollard::models::ContainerStatsResponse) -> (u64, u64) {
    let mut rx_bytes = 0u64;
    let mut tx_bytes = 0u64;

    if let Some(networks) = &stats.networks {
        for net in networks.values() {
            rx_bytes += net.rx_bytes.unwrap_or(0);
            tx_bytes += net.tx_bytes.unwrap_or(0);
        }
    }

    (rx_bytes, tx_bytes)
}

/// 应用管理服务 (Docker 模式)
pub struct AppService {
    config: AppManagerConfig,
    docker: Docker,
    /// 路径解析器缓存（单例）
    path_resolver: Cache<String, Arc<HostPathResolver>>,
    /// 应用信息存储（并发安全，支持遍历）
    apps: Arc<DashMap<String, AppInfo>>,
}

/// 生成容器名称
fn container_name(app_id: &str) -> String {
    format!("{}-{}", CONTAINER_PREFIX, app_id)
}

/// 生成应用目录路径（容器内）
fn app_workspace_path(app_id: &str) -> PathBuf {
    let path = PathBuf::from(WORKSPACE_ROOT).join(app_id);
    tracing::debug!(
        "[app_workspace_path] WORKSPACE_ROOT={}, app_id={}, result={:?}",
        WORKSPACE_ROOT,
        app_id,
        path
    );
    path
}

impl AppService {
    /// 创建新的应用管理服务
    pub async fn new(config: AppManagerConfig) -> Result<Self> {
        let docker = Docker::connect_with_local_defaults().context("连接 Docker 失败")?;

        // 路径解析器缓存（单例）
        let path_resolver: Cache<String, Arc<HostPathResolver>> =
            Cache::builder().max_capacity(1).build();

        // 初始化路径解析器
        match HostPathResolver::new().await {
            Ok(resolver) => {
                info!("路径解析器初始化成功");
                path_resolver.insert("default".to_string(), Arc::new(resolver));
            }
            Err(e) => {
                warn!("路径解析器初始化失败，将使用容器内路径: {}", e);
            }
        }

        Ok(Self {
            config,
            docker,
            path_resolver,
            apps: Arc::new(DashMap::new()),
        })
    }

    /// 创建应用
    #[instrument(skip(self, request))]
    pub async fn create_app(&self, request: CreateAppRequest) -> Result<AppInfo> {
        let app_id = format!("app-{}", &Uuid::new_v4().to_string()[..8]);
        info!("创建应用: {} ({})", request.name, app_id);

        // 创建应用目录（容器内路径）
        self.create_app_dirs(&app_id).await?;

        // 获取宿主机路径（用于容器挂载）
        let host_app_dir = self.get_host_app_dir(&app_id);
        info!("宿主机应用目录: {:?}", host_app_dir);

        // 构建应用信息
        let now = Utc::now().to_rfc3339();
        let app_info = AppInfo {
            app_id: app_id.clone(),
            name: request.name.clone(),
            status: AppStatus::Created,
            image: request.image.clone(),
            command: request.command.clone().unwrap_or_default(),
            replicas: 0,
            access: self.build_access_info(&app_id, &request.ports),
            health: HealthInfo {
                status: "Unknown".to_string(),
                instance: None,
                probes: None,
            },
            resources: request.resources.clone(),
            env: request.env.clone().unwrap_or_default(),
            created_at: now.clone(),
            updated_at: now,
        };

        // 创建容器
        let container_name = container_name(&app_id);
        let container_config = self.build_container_config(&app_id, &request);

        match self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name.clone()),
                    platform: String::new(),
                }),
                container_config,
            )
            .await
        {
            Ok(response) => {
                info!("容器创建成功: {}", response.id);
            }
            Err(e) => {
                error!("容器创建失败: {}", e);
                return Err(anyhow::anyhow!("容器创建失败: {}", e));
            }
        }

        // 保存应用信息
        self.apps.insert(app_id.clone(), app_info.clone());

        Ok(app_info)
    }

    /// 查询应用列表
    #[instrument(skip(self, request))]
    pub async fn query_apps(
        &self,
        request: QueryAppsRequest,
    ) -> Result<PaginatedResponse<AppInfo>> {
        let mut items: Vec<AppInfo> = self.apps.iter().map(|r| r.value().clone()).collect();

        // 过滤
        if let Some(filters) = &request.filters {
            if let Some(status) = &filters.status {
                items.retain(|app| status.contains(&app.status));
            }
            if let Some(name) = &filters.name {
                items.retain(|app| app.name.contains(name));
            }
            if let Some(app_ids) = &filters.app_ids {
                items.retain(|app| app_ids.contains(&app.app_id));
            }
        }

        // 排序
        if let Some(sort_by) = &request.sort_by {
            match sort_by.as_str() {
                "name" => items.sort_by(|a, b| a.name.cmp(&b.name)),
                "created_at" => items.sort_by(|a, b| a.created_at.cmp(&b.created_at)),
                _ => {}
            }
            if request.sort_order == Some(SortOrder::Asc) {
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

    /// 获取应用详情
    #[instrument(skip(self))]
    pub async fn get_app(&self, app_id: &str) -> Result<AppInfo> {
        self.apps
            .get(app_id)
            .map(|r| r.value().clone())
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))
    }

    /// 更新应用配置
    #[instrument(skip(self, request))]
    pub async fn update_app(&self, app_id: &str, request: UpdateAppRequest) -> Result<AppInfo> {
        let mut entry = self
            .apps
            .get_mut(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        let app = entry.value_mut();

        if let Some(name) = request.name {
            app.name = name;
        }
        if let Some(image) = request.image {
            app.image = image;
        }
        if let Some(command) = request.command {
            app.command = command;
        }
        if let Some(env) = request.env {
            app.env.extend(env);
        }
        if let Some(resources) = request.resources {
            app.resources = Some(resources);
        }

        app.updated_at = Utc::now().to_rfc3339();

        Ok(app.clone())
    }

    /// 删除应用
    #[instrument(skip(self))]
    pub async fn delete_app(&self, app_id: &str) -> Result<()> {
        // 停止并删除容器
        let container_name = container_name(app_id);
        let _ = self
            .docker
            .remove_container(
                &container_name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await;

        // 删除应用信息
        self.apps
            .remove(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        // 删除应用目录（容器内路径）
        let app_dir = self.get_container_app_dir(app_id);
        if app_dir.exists() {
            fs::remove_dir_all(&app_dir).await?;
        }

        Ok(())
    }

    /// 启动应用
    #[instrument(skip(self))]
    pub async fn start_app(&self, app_id: &str) -> Result<AppInfo> {
        let mut entry = self
            .apps
            .get_mut(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        let app = entry.value_mut();

        if app.status == AppStatus::Running {
            return Err(anyhow::anyhow!("应用已在运行"));
        }

        // 启动容器
        let container_name = container_name(app_id);
        match self
            .docker
            .start_container(&container_name, None::<StartContainerOptions>)
            .await
        {
            Ok(_) => {
                info!("容器启动成功: {}", container_name);
                app.status = AppStatus::Running;
                app.replicas = 1;
                app.updated_at = Utc::now().to_rfc3339();
            }
            Err(e) => {
                error!("容器启动失败: {}", e);
                app.status = AppStatus::Error;
                return Err(anyhow::anyhow!("容器启动失败: {}", e));
            }
        }

        Ok(app.clone())
    }

    /// 停止应用
    #[instrument(skip(self))]
    pub async fn stop_app(&self, app_id: &str) -> Result<AppInfo> {
        let mut entry = self
            .apps
            .get_mut(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        let app = entry.value_mut();

        if app.status != AppStatus::Running {
            return Err(anyhow::anyhow!("应用未在运行"));
        }

        // 停止容器
        let container_name = container_name(app_id);
        match self
            .docker
            .stop_container(
                &container_name,
                Some(StopContainerOptions {
                    t: Some(10),
                    signal: Some(String::new()),
                }),
            )
            .await
        {
            Ok(_) => {
                info!("容器停止成功: {}", container_name);
                app.status = AppStatus::Stopped;
                app.replicas = 0;
                app.updated_at = Utc::now().to_rfc3339();
            }
            Err(e) => {
                error!("容器停止失败: {}", e);
                return Err(anyhow::anyhow!("容器停止失败: {}", e));
            }
        }

        Ok(app.clone())
    }

    /// 重启应用
    #[instrument(skip(self))]
    pub async fn restart_app(&self, app_id: &str) -> Result<AppInfo> {
        let mut entry = self
            .apps
            .get_mut(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        let app = entry.value_mut();

        // 重启容器
        let container_name = container_name(app_id);

        // 先停止
        let _ = self
            .docker
            .stop_container(
                &container_name,
                Some(StopContainerOptions {
                    t: Some(10),
                    signal: Some(String::new()),
                }),
            )
            .await;

        // 再启动
        match self
            .docker
            .start_container(&container_name, None::<StartContainerOptions>)
            .await
        {
            Ok(_) => {
                info!("容器重启成功: {}", container_name);
                app.status = AppStatus::Running;
                app.replicas = 1;
                app.updated_at = Utc::now().to_rfc3339();
            }
            Err(e) => {
                error!("容器重启失败: {}", e);
                app.status = AppStatus::Error;
                return Err(anyhow::anyhow!("容器重启失败: {}", e));
            }
        }

        Ok(app.clone())
    }

    /// 获取应用日志
    #[instrument(skip(self))]
    pub async fn get_app_logs(&self, app_id: &str, params: LogParams) -> Result<Vec<LogEntry>> {
        let _app = self.get_app(app_id).await?;

        let log_dir = self.get_container_app_dir(app_id).join("logs");
        let log_file = log_dir.join("app.log");

        if !log_file.exists() {
            return Ok(vec![]);
        }

        let content = fs::read_to_string(&log_file).await?;
        let tail = params.tail.unwrap_or(1000) as usize;
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(tail);

        let entries = lines[start..]
            .iter()
            .map(|line| LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                stream: "stdout".to_string(),
                message: line.to_string(),
            })
            .collect();

        Ok(entries)
    }

    /// 上传文件
    #[instrument(skip(self, file_data))]
    pub async fn upload_file(
        &self,
        app_id: &str,
        file_data: Vec<u8>,
        target: &str,
    ) -> Result<UploadResult> {
        let _app = self.get_app(app_id).await?;

        let app_dir = self.get_container_app_dir(app_id);
        let file_path = app_dir.join("code").join(target);

        // 确保父目录存在
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
        let _app = self.get_app(app_id).await?;

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
        let _app = self.get_app(app_id).await?;

        let app_dir = self.get_container_app_dir(app_id);
        let full_path = app_dir.join("code").join(file_path);

        // 安全检查：确保路径在应用目录内
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

        info!("文件已删除: {}", file_path);
        Ok(())
    }

    /// 获取资源使用情况
    #[instrument(skip(self))]
    pub async fn get_app_stats(&self, app_id: &str) -> Result<ResourceStats> {
        let _app = self.get_app(app_id).await?;

        let container_name = container_name(app_id);

        // 使用 Docker stats API 获取实时资源使用
        use bollard::query_parameters::StatsOptionsBuilder;

        let options = StatsOptionsBuilder::default()
            .stream(false)
            .one_shot(true)
            .build();

        let mut stats_stream = self.docker.stats(&container_name, Some(options));

        if let Some(result) = stats_stream.next().await {
            match result {
                Ok(stats) => {
                    // 计算 CPU 使用率
                    let cpu_percent = calculate_cpu_percent(&stats);

                    // 内存使用
                    let memory_stats = stats.memory_stats.as_ref();
                    let memory_usage = memory_stats.and_then(|m| m.usage).unwrap_or(0);
                    let memory_limit = memory_stats.and_then(|m| m.limit).unwrap_or(0);
                    let memory_percent = if memory_limit > 0 {
                        (memory_usage as f64 / memory_limit as f64) * 100.0
                    } else {
                        0.0
                    };

                    // 网络使用
                    let (rx_bytes, tx_bytes) = calculate_network_usage(&stats);

                    Ok(ResourceStats {
                        cpu: CpuStats {
                            usage_percent: cpu_percent,
                            usage_cores: cpu_percent / 100.0,
                            limit_cores: 1.0,
                        },
                        memory: MemoryStats {
                            usage_bytes: memory_usage,
                            usage_percent: memory_percent,
                            limit_bytes: memory_limit,
                        },
                        network: NetworkStats { rx_bytes, tx_bytes },
                        restart_count: 0,
                    })
                }
                Err(e) => {
                    warn!("查询容器资源失败: {}", e);
                    Err(anyhow::anyhow!("查询容器资源失败: {}", e))
                }
            }
        } else {
            Err(anyhow::anyhow!("无法获取容器资源信息"))
        }
    }

    /// 获取应用事件
    #[instrument(skip(self))]
    pub async fn get_app_events(&self, app_id: &str) -> Result<Vec<String>> {
        let _app = self.get_app(app_id).await?;

        // Docker 模式下，可以从容器状态变化中提取事件
        // 暂时返回空列表
        Ok(vec![])
    }

    // ============================================================================
    // 辅助方法
    // ============================================================================

    /// 获取应用目录（宿主机路径）
    ///
    /// 使用缓存的路径解析器将容器内路径转换为宿主机路径
    fn get_host_app_dir(&self, app_id: &str) -> PathBuf {
        let container_path = app_workspace_path(app_id);
        tracing::debug!(
            "[get_host_app_dir] app_id={}, container_path={:?}",
            app_id,
            container_path
        );

        // 从缓存获取路径解析器
        if let Some(resolver) = self.path_resolver.get("default") {
            let result = resolver.resolve_to_host_path(&container_path);
            tracing::debug!("[get_host_app_dir] resolve result={:?}", result);
            result.unwrap_or(container_path)
        } else {
            tracing::warn!("[get_host_app_dir] path_resolver not available, using container_path");
            container_path
        }
    }

    /// 获取应用目录（容器内路径）
    fn get_container_app_dir(&self, app_id: &str) -> PathBuf {
        PathBuf::from(self.config.get_workspace_root()).join(app_id)
    }

    /// 创建应用目录
    async fn create_app_dirs(&self, app_id: &str) -> Result<()> {
        let app_dir = self.get_container_app_dir(app_id);
        fs::create_dir_all(app_dir.join("code")).await?;
        fs::create_dir_all(app_dir.join("data")).await?;
        fs::create_dir_all(app_dir.join("logs")).await?;
        Ok(())
    }

    /// 构建容器配置
    fn build_container_config(
        &self,
        app_id: &str,
        request: &CreateAppRequest,
    ) -> ContainerCreateBody {
        // 使用宿主机路径进行挂载
        let host_app_dir = self.get_host_app_dir(app_id);

        // 构建挂载点
        let mounts = vec![Mount {
            target: Some("/app".to_string()),
            source: Some(host_app_dir.to_string_lossy().to_string()),
            typ: Some(MountType::BIND),
            ..Default::default()
        }];

        // 构建环境变量
        let env: Vec<String> = request
            .env
            .as_ref()
            .map(|env| env.iter().map(|(k, v)| format!("{}={}", k, v)).collect())
            .unwrap_or_default();

        // 构建主机配置
        let host_config = HostConfig {
            mounts: Some(mounts),
            ..Default::default()
        };

        ContainerCreateBody {
            image: Some(request.image.clone()),
            cmd: request.command.clone(),
            env: Some(env),
            host_config: Some(host_config),
            ..Default::default()
        }
    }

    /// 构建访问信息
    fn build_access_info(&self, app_id: &str, ports: &Option<Vec<PortConfig>>) -> AccessInfo {
        let http_port = ports
            .as_ref()
            .and_then(|p| p.iter().find(|p| p.expose_type == ExposeType::Http));

        let tcp_ports: Vec<TcpPortMapping> = ports
            .as_ref()
            .map(|p| {
                p.iter()
                    .filter(|p| p.expose_type == ExposeType::Tcp)
                    .map(|p| TcpPortMapping {
                        name: p.name.clone(),
                        node_port: 0, // TODO: 查询实际 NodePort
                        access_url: format!("tcp://{}:0", self.config.get_node_ip()),
                    })
                    .collect()
            })
            .unwrap_or_default();

        AccessInfo {
            external: ExternalAccess {
                http: http_port.map(|_| {
                    format!(
                        "http://{}:{}/apps/{}",
                        self.config.get_node_ip(), self.config.get_gateway_node_port(), app_id
                    )
                }),
                tcp: tcp_ports,
            },
            internal: InternalAccess {
                // Docker 模式：使用容器名称作为内部域名
                domain: format!("rcoder-app-{}", app_id),
                short_domain: format!("rcoder-app-{}", app_id),
                ports: ports
                    .as_ref()
                    .map(|p| {
                        p.iter()
                            .map(|p| InternalPort {
                                name: p.name.clone(),
                                port: p.port,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            },
        }
    }
}

#[async_trait::async_trait]
impl super::AppServiceTrait for AppService {
    async fn create_app(&self, request: CreateAppRequest) -> Result<AppInfo> {
        self.create_app(request).await
    }

    async fn query_apps(&self, request: QueryAppsRequest) -> Result<PaginatedResponse<AppInfo>> {
        self.query_apps(request).await
    }

    async fn get_app(&self, app_id: &str) -> Result<AppInfo> {
        self.get_app(app_id).await
    }

    async fn update_app(&self, app_id: &str, request: UpdateAppRequest) -> Result<AppInfo> {
        self.update_app(app_id, request).await
    }

    async fn delete_app(&self, app_id: &str) -> Result<()> {
        self.delete_app(app_id).await
    }

    async fn start_app(&self, app_id: &str) -> Result<AppInfo> {
        self.start_app(app_id).await
    }

    async fn stop_app(&self, app_id: &str) -> Result<AppInfo> {
        self.stop_app(app_id).await
    }

    async fn restart_app(&self, app_id: &str) -> Result<AppInfo> {
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
