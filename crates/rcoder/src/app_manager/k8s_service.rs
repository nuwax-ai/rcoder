//! 应用管理服务层 - K8s 模式

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use tracing::{info, instrument, warn};
use uuid::Uuid;

use container_runtime_api::{ContainerCreateParams, ContainerRuntime, ContainerRuntimeStatus};

use super::config::AppManagerConfig;
use super::models::*;

/// K8s 应用管理服务
pub struct K8sAppService {
    config: AppManagerConfig,
    runtime: Arc<dyn ContainerRuntime>,
    apps: tokio::sync::RwLock<HashMap<String, AppInfo>>,
}

impl K8sAppService {
    /// 创建新的 K8s 应用管理服务
    pub fn new(config: AppManagerConfig, runtime: Arc<dyn ContainerRuntime>) -> Self {
        Self {
            config,
            runtime,
            apps: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    /// 创建应用
    #[instrument(skip(self, request))]
    pub async fn create_app(&self, request: CreateAppRequest) -> Result<AppInfo> {
        let app_id = format!("app-{}", &Uuid::new_v4().to_string()[..8]);
        info!("创建应用 (K8s): {} ({})", request.name, app_id);

        // 构建容器创建参数
        let params = self.build_container_params(&app_id, &request)?;

        // 创建容器（Pod）
        let container_info = self
            .runtime
            .create_container(params)
            .await
            .map_err(|e| anyhow::anyhow!("创建 Pod 失败: {}", e))?;

        info!("Pod 创建成功: {:?}", container_info);

        // 构建应用信息
        let now = Utc::now().to_rfc3339();
        let app_info = AppInfo {
            app_id: app_id.clone(),
            name: request.name.clone(),
            status: AppStatus::Running,
            image: request.image.clone(),
            command: request.command.clone().unwrap_or_default(),
            replicas: 1,
            access: self.build_access_info(&app_id, &request.ports),
            health: HealthInfo {
                status: "Starting".to_string(),
                instance: Some(InstanceInfo {
                    name: container_info.container_id.clone(),
                    phase: "Running".to_string(),
                    ready: false,
                    restart_count: 0,
                    node: String::new(),
                    ip: container_info.container_ip.clone(),
                    started_at: Some(now.clone()),
                }),
                probes: None,
            },
            resources: request.resources.clone(),
            env: request.env.clone().unwrap_or_default(),
            created_at: now.clone(),
            updated_at: now,
        };

        // 保存应用信息
        let mut apps = self.apps.write().await;
        apps.insert(app_id.clone(), app_info.clone());

        Ok(app_info)
    }

    /// 查询应用列表
    #[instrument(skip(self, request))]
    pub async fn query_apps(&self, request: QueryAppsRequest) -> Result<PaginatedResponse<AppInfo>> {
        let apps = self.apps.read().await;
        let mut items: Vec<AppInfo> = apps.values().cloned().collect();

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
        let apps = self.apps.read().await;
        apps.get(app_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))
    }

    /// 更新应用配置
    #[instrument(skip(self, request))]
    pub async fn update_app(&self, app_id: &str, request: UpdateAppRequest) -> Result<AppInfo> {
        let mut apps = self.apps.write().await;
        let app = apps
            .get_mut(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

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
        let _ = self
            .runtime
            .stop_container(app_id)
            .await;

        // 删除应用信息
        let mut apps = self.apps.write().await;
        apps.remove(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        Ok(())
    }

    /// 启动应用
    #[instrument(skip(self))]
    pub async fn start_app(&self, app_id: &str) -> Result<AppInfo> {
        let mut apps = self.apps.write().await;
        let app = apps
            .get_mut(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        if app.status == AppStatus::Running {
            return Err(anyhow::anyhow!("应用已在运行"));
        }

        // 检查容器状态
        let container_info = self
            .runtime
            .find_container(app_id, &shared_types::ServiceType::RCoder)
            .await
            .map_err(|e| anyhow::anyhow!("查询容器失败: {}", e))?;

        match container_info {
            Some(info) => {
                if info.status == ContainerRuntimeStatus::Running {
                    app.status = AppStatus::Running;
                    app.replicas = 1;
                    app.updated_at = Utc::now().to_rfc3339();
                } else {
                    return Err(anyhow::anyhow!("容器状态异常，请重新创建应用"));
                }
            }
            None => {
                return Err(anyhow::anyhow!("容器不存在，请重新创建应用"));
            }
        }

        Ok(app.clone())
    }

    /// 停止应用
    #[instrument(skip(self))]
    pub async fn stop_app(&self, app_id: &str) -> Result<AppInfo> {
        let mut apps = self.apps.write().await;
        let app = apps
            .get_mut(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        if app.status != AppStatus::Running {
            return Err(anyhow::anyhow!("应用未在运行"));
        }

        // 停止容器
        self.runtime
            .stop_container(app_id)
            .await
            .map_err(|e| anyhow::anyhow!("停止容器失败: {}", e))?;

        app.status = AppStatus::Stopped;
        app.replicas = 0;
        app.updated_at = Utc::now().to_rfc3339();

        Ok(app.clone())
    }

    /// 重启应用
    #[instrument(skip(self))]
    pub async fn restart_app(&self, app_id: &str) -> Result<AppInfo> {
        let mut apps = self.apps.write().await;
        let app = apps
            .get_mut(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        // 停止容器
        let _ = self.runtime.stop_container(app_id).await;

        // TODO: 实现容器重建逻辑

        app.status = AppStatus::Running;
        app.replicas = 1;
        app.updated_at = Utc::now().to_rfc3339();

        Ok(app.clone())
    }

    /// 获取应用日志
    #[instrument(skip(self))]
    pub async fn get_app_logs(&self, app_id: &str, params: LogParams) -> Result<Vec<LogEntry>> {
        let _app = self.get_app(app_id).await?;

        // 获取容器信息
        let _container_info = self
            .runtime
            .find_container(app_id, &shared_types::ServiceType::RCoder)
            .await
            .map_err(|e| anyhow::anyhow!("查询容器失败: {}", e))?
            .ok_or_else(|| anyhow::anyhow!("容器不存在"))?;

        // TODO: 实现 K8s Pod 日志查询
        // 需要使用 kube-rs API 查询 Pod 日志
        // kubectl logs <pod-name> -n <namespace> --tail=<lines>
        warn!("K8s 日志查询功能待完整实现");

        // 暂时返回空日志
        Ok(vec![])
    }

    /// 列出文件
    #[instrument(skip(self))]
    pub async fn list_files(&self, app_id: &str) -> Result<Vec<FileInfo>> {
        let _app = self.get_app(app_id).await?;

        // K8s 模式下，文件存储在 PVC 中
        // 可以通过以下方式访问：
        // 1. kubectl cp 命令
        // 2. Pod exec + ls 命令
        // 3. 共享存储（NFS/CephFS）直接访问
        warn!("K8s 文件管理功能待实现");

        Ok(vec![])
    }

    // ============================================================================
    // 辅助方法
    // ============================================================================

    /// 构建容器创建参数
    fn build_container_params(
        &self,
        app_id: &str,
        _request: &CreateAppRequest,
    ) -> Result<ContainerCreateParams> {
        let params = ContainerCreateParams::builder()
            .project_id(app_id.to_string())
            .service_type(shared_types::ServiceType::RCoder)
            .host_workspace_path(format!("{}/{}", self.config.workspace_root, app_id))
            .build();

        Ok(params)
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
                        node_port: 0,
                        access_url: format!("tcp://{}:0", self.config.node_ip),
                    })
                    .collect()
            })
            .unwrap_or_default();

        AccessInfo {
            external: ExternalAccess {
                http: http_port.map(|_| {
                    format!(
                        "http://{}:{}/apps/{}",
                        self.config.node_ip, self.config.gateway_node_port, app_id
                    )
                }),
                tcp: tcp_ports,
            },
            internal: InternalAccess {
                domain: format!(
                    "{}-svc.{}.svc.cluster.local",
                    app_id, self.config.namespace
                ),
                short_domain: format!("{}-svc.{}", app_id, self.config.namespace),
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
impl super::AppServiceTrait for K8sAppService {
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

    async fn get_app_stats(&self, _app_id: &str) -> Result<ResourceStats> {
        // K8s 模式下资源使用查询待实现
        warn!("K8s 资源使用查询功能待实现");
        Ok(ResourceStats {
            cpu: CpuStats {
                usage_percent: 0.0,
                usage_cores: 0.0,
                limit_cores: 0.0,
            },
            memory: MemoryStats {
                usage_bytes: 0,
                usage_percent: 0.0,
                limit_bytes: 0,
            },
            network: NetworkStats {
                rx_bytes: 0,
                tx_bytes: 0,
            },
            restart_count: 0,
        })
    }

    async fn get_app_events(&self, _app_id: &str) -> Result<Vec<String>> {
        // K8s 模式下事件查询待实现
        warn!("K8s 事件查询功能待实现");
        Ok(vec![])
    }

    async fn list_files(&self, app_id: &str) -> Result<Vec<FileInfo>> {
        self.list_files(app_id).await
    }
}
