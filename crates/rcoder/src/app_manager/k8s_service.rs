//! 应用管理服务层 - K8s 模式

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use tracing::{info, instrument, warn};
use uuid::Uuid;

use container_runtime_api::{ContainerCreateParams, ContainerRuntime};

use super::config::AppManagerConfig;
use super::models::*;

// K8s API 类型（仅在 kubernetes feature 启用时使用）
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::apps::v1::Deployment;
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    ConfigMap, Event, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, Secret, Service,
    ServicePort, ServiceSpec, VolumeResourceRequirements,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
#[cfg(feature = "kubernetes")]
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ListParams,
    LogParams as KubeLogParams, Patch, PatchParams, PostParams,
};
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;

/// 解析 K8s 内存数量（如 "512Mi", "1Gi"）为字节
#[cfg(feature = "kubernetes")]
fn parse_memory_quantity(quantity: &str) -> Option<u64> {
    let quantity = quantity.trim();

    if let Some(num_str) = quantity.strip_suffix("Ki") {
        num_str.parse::<f64>().ok().map(|n| (n * 1024.0) as u64)
    } else if let Some(num_str) = quantity.strip_suffix("Mi") {
        num_str
            .parse::<f64>()
            .ok()
            .map(|n| (n * 1024.0 * 1024.0) as u64)
    } else if let Some(num_str) = quantity.strip_suffix("Gi") {
        num_str
            .parse::<f64>()
            .ok()
            .map(|n| (n * 1024.0 * 1024.0 * 1024.0) as u64)
    } else if let Some(num_str) = quantity.strip_suffix("Ti") {
        num_str
            .parse::<f64>()
            .ok()
            .map(|n| (n * 1024.0 * 1024.0 * 1024.0 * 1024.0) as u64)
    } else {
        // 字节数
        quantity.parse::<u64>().ok()
    }
}

/// K8s 应用管理服务
pub struct K8sAppService {
    config: AppManagerConfig,
    runtime: Arc<dyn ContainerRuntime>,
    apps: tokio::sync::RwLock<HashMap<String, AppInfo>>,
    #[cfg(feature = "kubernetes")]
    kube_client: Option<kube::Client>,
}

impl K8sAppService {
    /// 创建新的 K8s 应用管理服务
    pub async fn new(config: AppManagerConfig, runtime: Arc<dyn ContainerRuntime>) -> Result<Self> {
        #[cfg(feature = "kubernetes")]
        let kube_client = match kube::Client::try_default().await {
            Ok(client) => Some(client),
            Err(e) => {
                warn!("Failed to create K8s client: {}", e);
                None
            }
        };

        Ok(Self {
            config,
            runtime,
            apps: tokio::sync::RwLock::new(HashMap::new()),
            #[cfg(feature = "kubernetes")]
            kube_client,
        })
    }

    /// 创建应用
    #[instrument(skip(self, request))]
    pub async fn create_app(&self, request: CreateAppRequest) -> Result<AppInfo> {
        let app_id = format!("app-{}", &Uuid::new_v4().to_string()[..8]);
        info!("创建应用 (K8s): {} ({})", request.name, app_id);

        // 1. 创建 PVC（如果需要持久化存储）
        #[cfg(feature = "kubernetes")]
        if let Some(resources) = &request.resources
            && let Some(storage) = &resources.storage
        {
            self.create_pvc(&app_id, storage).await?;
        }

        // 2. 构建容器创建参数
        let params = self.build_container_params(&app_id, &request)?;

        // 3. 创建容器（Pod/Deployment）
        let container_info = self
            .runtime
            .create_container(params)
            .await
            .map_err(|e| anyhow::anyhow!("创建 Pod 失败: {}", e))?;

        info!("Pod 创建成功: {:?}", container_info);

        // 4. 创建 Service
        // K8s 模式下，Service 由 ContainerRuntime 自动创建

        // 5. 创建 HTTPRoute（HTTP 端口）
        #[cfg(feature = "kubernetes")]
        if let Some(ports) = &request.ports
            && let Some(http_port) = ports.iter().find(|p| p.expose_type == ExposeType::Http)
        {
            self.create_httproute(&app_id, http_port.port).await?;
        }

        // 6. 创建 NodePort Service（TCP 端口）
        #[cfg(feature = "kubernetes")]
        {
            let tcp_ports = if let Some(ports) = &request.ports {
                self.create_nodeport_service(&app_id, ports).await?
            } else {
                vec![]
            };

            // 更新访问信息中的 TCP 端口
            if !tcp_ports.is_empty() {
                // 保存到应用信息中
            }
        }

        // 构建应用信息
        let now = Utc::now().to_rfc3339();
        let access = self.build_access_info(&app_id, &request.ports);

        let app_info = AppInfo {
            app_id: app_id.clone(),
            name: request.name.clone(),
            status: AppStatus::Running,
            image: request.image.clone(),
            command: request.command.clone().unwrap_or_default(),
            replicas: 1,
            access,
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
    pub async fn query_apps(
        &self,
        request: QueryAppsRequest,
    ) -> Result<PaginatedResponse<AppInfo>> {
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
        // 1. 停止并删除容器
        let _ = self.runtime.stop_container(app_id).await;

        // 2. 删除 K8s 资源（Deployment、Service、HTTPRoute、PVC 等）
        #[cfg(feature = "kubernetes")]
        self.delete_k8s_resources(app_id).await?;

        // 3. 删除应用信息
        let mut apps = self.apps.write().await;
        apps.remove(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        Ok(())
    }

    /// 启动应用（scale replicas > 0）
    #[instrument(skip(self))]
    pub async fn start_app(&self, app_id: &str) -> Result<AppInfo> {
        let mut apps = self.apps.write().await;
        let app = apps
            .get_mut(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        if app.status == AppStatus::Running {
            return Err(anyhow::anyhow!("应用已在运行"));
        }

        // K8s 模式：scale Deployment replicas = 1
        #[cfg(feature = "kubernetes")]
        self.scale_deployment(app_id, 1).await?;

        app.status = AppStatus::Running;
        app.replicas = 1;
        app.updated_at = Utc::now().to_rfc3339();

        Ok(app.clone())
    }

    /// 停止应用（scale replicas = 0）
    #[instrument(skip(self))]
    pub async fn stop_app(&self, app_id: &str) -> Result<AppInfo> {
        let mut apps = self.apps.write().await;
        let app = apps
            .get_mut(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        if app.status != AppStatus::Running {
            return Err(anyhow::anyhow!("应用未在运行"));
        }

        // K8s 模式：scale Deployment replicas = 0
        #[cfg(feature = "kubernetes")]
        self.scale_deployment(app_id, 0).await?;

        app.status = AppStatus::Stopped;
        app.replicas = 0;
        app.updated_at = Utc::now().to_rfc3339();

        Ok(app.clone())
    }

    /// 重启应用（滚动重启）
    #[instrument(skip(self))]
    pub async fn restart_app(&self, app_id: &str) -> Result<AppInfo> {
        let mut apps = self.apps.write().await;
        let app = apps
            .get_mut(app_id)
            .ok_or_else(|| anyhow::anyhow!("应用不存在: {}", app_id))?;

        // K8s 模式：触发滚动重启
        #[cfg(feature = "kubernetes")]
        self.restart_deployment(app_id).await?;

        app.status = AppStatus::Running;
        app.replicas = 1;
        app.updated_at = Utc::now().to_rfc3339();

        Ok(app.clone())
    }

    /// Scale Deployment
    #[cfg(feature = "kubernetes")]
    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> Result<()> {
        if let Some(client) = &self.kube_client {
            let deployments: Api<Deployment> =
                Api::namespaced(client.clone(), &self.config.namespace);
            let name = format!("app-{}", app_id);

            let patch = serde_json::json!({
                "spec": {
                    "replicas": replicas
                }
            });

            deployments
                .patch(&name, &PatchParams::default(), &Patch::Merge(patch))
                .await?;
            info!("Deployment {} scaled to {} replicas", name, replicas);
        }

        Ok(())
    }

    /// 触发滚动重启（通过更新 annotation）
    #[cfg(feature = "kubernetes")]
    async fn restart_deployment(&self, app_id: &str) -> Result<()> {
        if let Some(client) = &self.kube_client {
            let deployments: Api<Deployment> =
                Api::namespaced(client.clone(), &self.config.namespace);
            let name = format!("app-{}", app_id);

            let patch = serde_json::json!({
                "spec": {
                    "template": {
                        "metadata": {
                            "annotations": {
                                "kubectl.kubernetes.io/restartedAt": Utc::now().to_rfc3339()
                            }
                        }
                    }
                }
            });

            deployments
                .patch(&name, &PatchParams::default(), &Patch::Merge(patch))
                .await?;
            info!("Deployment {} restarted", name);
        }

        Ok(())
    }

    /// 获取应用日志
    #[instrument(skip(self))]
    pub async fn get_app_logs(&self, app_id: &str, params: LogParams) -> Result<Vec<LogEntry>> {
        let _app = self.get_app(app_id).await?;

        #[cfg(feature = "kubernetes")]
        {
            if let Some(client) = &self.kube_client {
                let pods: Api<Pod> = Api::namespaced(client.clone(), &self.config.namespace);

                // 查找 Pod
                let pod_name = format!("app-{}", app_id);
                let lp = ListParams::default().labels(&format!("app={}", pod_name));
                let pod_list = pods.list(&lp).await?;

                if let Some(pod) = pod_list.items.first() {
                    let pod_name = pod.metadata.name.clone().unwrap_or_default();

                    // 查询日志
                    let log_params = KubeLogParams {
                        tail_lines: Some(params.tail.unwrap_or(1000) as i64),
                        timestamps: true,
                        ..Default::default()
                    };

                    let logs = pods.logs(&pod_name, &log_params).await?;

                    // 解析日志
                    let entries: Vec<LogEntry> = logs
                        .lines()
                        .map(|line| LogEntry {
                            timestamp: Utc::now().to_rfc3339(),
                            stream: "stdout".to_string(),
                            message: line.to_string(),
                        })
                        .collect();

                    return Ok(entries);
                }
            }
        }

        warn!("K8s 日志查询功能不可用");
        Ok(vec![])
    }

    /// 列出文件
    #[instrument(skip(self))]
    pub async fn list_files(&self, app_id: &str) -> Result<Vec<FileInfo>> {
        let _app = self.get_app(app_id).await?;

        // K8s 模式下，文件管理通过以下方式实现：
        // 1. PVC 共享存储：RCoder 和应用共享 PVC，直接读写
        // 2. 应用内 API：应用提供文件管理 API，RCoder 通过 HTTP 调用
        //
        // 当前实现：读取 PVC 挂载目录
        let code_dir = format!("{}/{}/code", self.config.workspace_root, app_id);

        // 检查目录是否存在（如果 PVC 已挂载）
        let path = std::path::Path::new(&code_dir);
        if !path.exists() {
            warn!("K8s 文件目录不存在: {} (需要挂载 PVC)", code_dir);
            return Ok(vec![]);
        }

        // 读取目录内容
        let mut files = Vec::new();
        let mut entries = tokio::fs::read_dir(&code_dir).await?;

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

        let code_dir = format!("{}/{}/code", self.config.workspace_root, app_id);
        let full_path = std::path::Path::new(&code_dir).join(file_path);

        // 安全检查：确保路径在应用目录内
        let canonical_path = full_path.canonicalize()?;
        let code_dir_canonical = std::path::Path::new(&code_dir).canonicalize()?;
        if !canonical_path.starts_with(&code_dir_canonical) {
            return Err(anyhow::anyhow!("路径不在应用目录内"));
        }

        if !canonical_path.exists() {
            return Err(anyhow::anyhow!("文件不存在: {}", file_path));
        }

        if canonical_path.is_dir() {
            tokio::fs::remove_dir_all(&canonical_path).await?;
        } else {
            tokio::fs::remove_file(&canonical_path).await?;
        }

        info!("文件已删除: {}", file_path);
        Ok(())
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

        // K8s 模式下，文件上传通过以下方式实现：
        // 1. PVC 共享存储：RCoder 和应用共享 PVC，直接写入
        // 2. 应用内 API：应用提供文件上传 API，RCoder 通过 HTTP 调用
        //
        // 当前实现：写入 PVC 挂载目录
        let code_dir = format!("{}/{}/code", self.config.workspace_root, app_id);
        let file_path = format!("{}/{}", code_dir, target);

        // 确保目录存在
        if let Some(parent) = std::path::Path::new(&file_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // 写入文件
        tokio::fs::write(&file_path, &file_data).await?;

        info!("文件上传成功: {} ({} bytes)", file_path, file_data.len());

        Ok(UploadResult {
            file_path: target.to_string(),
            file_size: file_data.len() as u64,
            uploaded_at: Utc::now().to_rfc3339(),
        })
    }

    // ============================================================================
    // K8s 资源创建方法
    // ============================================================================

    /// 创建 HTTPRoute
    #[cfg(feature = "kubernetes")]
    async fn create_httproute(&self, app_id: &str, port: u16) -> Result<()> {
        if let Some(client) = &self.kube_client {
            // 使用 GroupVersionKind 动态获取 HTTPRoute 资源类型
            let gvk = GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute");
            let api_resource = ApiResource::from_gvk(&gvk);

            let httproutes: Api<DynamicObject> =
                Api::namespaced_with(client.clone(), &self.config.namespace, &api_resource);

            let httproute = serde_json::json!({
                "apiVersion": "gateway.networking.k8s.io/v1",
                "kind": "HTTPRoute",
                "metadata": {
                    "name": format!("app-{}-route", app_id),
                    "namespace": self.config.namespace,
                    "labels": {
                        "app": format!("app-{}", app_id),
                        "managed-by": "rcoder"
                    }
                },
                "spec": {
                    "parentRefs": [{
                        "name": self.config.gateway_name,
                        "namespace": self.config.gateway_namespace
                    }],
                    "rules": [{
                        "matches": [{
                            "path": {
                                "type": "PathPrefix",
                                "value": format!("/apps/{}", app_id)
                            }
                        }],
                        "backendRefs": [{
                            "name": format!("app-{}-svc", app_id),
                            "port": port
                        }]
                    }]
                }
            });

            let params = PostParams::default();
            httproutes
                .create(&params, &serde_json::from_value(httproute)?)
                .await?;
            info!("HTTPRoute created for app: {}", app_id);
        }

        Ok(())
    }

    /// 创建 NodePort Service（用于 TCP 端口暴露）
    #[cfg(feature = "kubernetes")]
    async fn create_nodeport_service(
        &self,
        app_id: &str,
        ports: &[PortConfig],
    ) -> Result<Vec<TcpPortMapping>> {
        let mut tcp_ports = Vec::new();

        if let Some(client) = &self.kube_client {
            let services: Api<Service> = Api::namespaced(client.clone(), &self.config.namespace);

            let tcp_port_configs: Vec<_> = ports
                .iter()
                .filter(|p| p.expose_type == ExposeType::Tcp)
                .collect();

            if tcp_port_configs.is_empty() {
                return Ok(tcp_ports);
            }

            let service_ports: Vec<ServicePort> = tcp_port_configs
                .iter()
                .map(|p| ServicePort {
                    name: Some(p.name.clone()),
                    port: p.port as i32,
                    target_port: Some(IntOrString::Int(p.port as i32)),
                    ..Default::default()
                })
                .collect();

            let service = Service {
                metadata: ObjectMeta {
                    name: Some(format!("app-{}-nodeport", app_id)),
                    namespace: Some(self.config.namespace.clone()),
                    labels: Some(BTreeMap::from([
                        ("app".to_string(), format!("app-{}", app_id)),
                        ("managed-by".to_string(), "rcoder".to_string()),
                    ])),
                    ..Default::default()
                },
                spec: Some(ServiceSpec {
                    type_: Some("NodePort".to_string()),
                    selector: Some(BTreeMap::from([(
                        "app".to_string(),
                        format!("app-{}", app_id),
                    )])),
                    ports: Some(service_ports),
                    ..Default::default()
                }),
                ..Default::default()
            };

            let created = services.create(&PostParams::default(), &service).await?;

            // 获取分配的 NodePort
            if let Some(spec) = created.spec
                && let Some(svc_ports) = spec.ports
            {
                for (i, port) in svc_ports.iter().enumerate() {
                    if let Some(node_port) = port.node_port {
                        tcp_ports.push(TcpPortMapping {
                            name: tcp_port_configs
                                .get(i)
                                .map(|p| p.name.clone())
                                .unwrap_or_default(),
                            node_port: node_port as u16,
                            access_url: format!("tcp://{}:{}", self.config.node_ip, node_port),
                        });
                    }
                }
            }

            info!(
                "NodePort Service created for app: {}, ports: {:?}",
                app_id, tcp_ports
            );
        }

        Ok(tcp_ports)
    }

    /// 创建 PVC
    #[cfg(feature = "kubernetes")]
    async fn create_pvc(&self, app_id: &str, storage_size: &str) -> Result<()> {
        if let Some(client) = &self.kube_client {
            let pvcs: Api<PersistentVolumeClaim> =
                Api::namespaced(client.clone(), &self.config.namespace);

            let pvc = PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some(format!("app-{}-data", app_id)),
                    namespace: Some(self.config.namespace.clone()),
                    labels: Some(BTreeMap::from([
                        ("app".to_string(), format!("app-{}", app_id)),
                        ("managed-by".to_string(), "rcoder".to_string()),
                    ])),
                    ..Default::default()
                },
                spec: Some(PersistentVolumeClaimSpec {
                    access_modes: Some(vec!["ReadWriteOnce".to_string()]),
                    storage_class_name: Some(self.config.storage_class.clone()),
                    resources: Some(VolumeResourceRequirements {
                        requests: Some(BTreeMap::from([(
                            "storage".to_string(),
                            Quantity(storage_size.to_string()),
                        )])),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            };

            pvcs.create(&PostParams::default(), &pvc).await?;
            info!("PVC created for app: {}", app_id);
        }

        Ok(())
    }

    /// 删除应用的所有 K8s 资源
    #[cfg(feature = "kubernetes")]
    async fn delete_k8s_resources(&self, app_id: &str) -> Result<()> {
        if let Some(client) = &self.kube_client {
            let name = format!("app-{}", app_id);

            // 删除 Deployment
            let deployments: Api<Deployment> =
                Api::namespaced(client.clone(), &self.config.namespace);
            let _ = deployments.delete(&name, &DeleteParams::default()).await;

            // 删除 Service
            let services: Api<Service> = Api::namespaced(client.clone(), &self.config.namespace);
            let _ = services
                .delete(&format!("{}-svc", name), &DeleteParams::default())
                .await;
            let _ = services
                .delete(&format!("{}-nodeport", name), &DeleteParams::default())
                .await;

            // 删除 HTTPRoute
            let gvk = GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute");
            let api_resource = ApiResource::from_gvk(&gvk);
            let httproutes: Api<DynamicObject> =
                Api::namespaced_with(client.clone(), &self.config.namespace, &api_resource);
            let _ = httproutes
                .delete(&format!("{}-route", name), &DeleteParams::default())
                .await;

            // 删除 PVC
            let pvcs: Api<PersistentVolumeClaim> =
                Api::namespaced(client.clone(), &self.config.namespace);
            let _ = pvcs
                .delete(&format!("{}-data", name), &DeleteParams::default())
                .await;

            // 删除 ConfigMap
            let configmaps: Api<ConfigMap> =
                Api::namespaced(client.clone(), &self.config.namespace);
            let _ = configmaps
                .delete(&format!("{}-config", name), &DeleteParams::default())
                .await;

            // 删除 Secret
            let secrets: Api<Secret> = Api::namespaced(client.clone(), &self.config.namespace);
            let _ = secrets
                .delete(&format!("{}-secret", name), &DeleteParams::default())
                .await;

            info!("K8s resources deleted for app: {}", app_id);
        }

        Ok(())
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
            .service_type(shared_types::ServiceType::WebAgentRunner)
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
                domain: format!("{}-svc.{}.svc.cluster.local", app_id, self.config.namespace),
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

    async fn get_app_stats(&self, app_id: &str) -> Result<ResourceStats> {
        #[cfg(feature = "kubernetes")]
        {
            if let Some(client) = &self.kube_client {
                let pods: Api<Pod> = Api::namespaced(client.clone(), &self.config.namespace);

                // 查找 Pod
                let lp = ListParams::default().labels(&format!("app=app-{}", app_id));
                let pod_list = pods.list(&lp).await?;

                if let Some(pod) = pod_list.items.first() {
                    // 获取 Pod 资源使用（需要 Metrics Server）
                    // 这里返回从 Pod spec 中获取的资源配置
                    let containers = pod.spec.as_ref().and_then(|s| s.containers.first());

                    let (cpu_limit, memory_limit) = containers
                        .and_then(|c| c.resources.as_ref())
                        .and_then(|r| r.limits.as_ref())
                        .map(|l| {
                            let cpu = l
                                .get("cpu")
                                .and_then(|q| q.0.parse::<f64>().ok())
                                .unwrap_or(0.0);
                            let memory = l
                                .get("memory")
                                .and_then(|q| parse_memory_quantity(&q.0))
                                .unwrap_or(0);
                            (cpu, memory)
                        })
                        .unwrap_or((0.0, 0));

                    // 获取重启次数
                    let restart_count = pod
                        .status
                        .as_ref()
                        .and_then(|s| s.container_statuses.as_ref())
                        .and_then(|cs| cs.first())
                        .map(|cs| cs.restart_count)
                        .unwrap_or(0) as u32;

                    // TODO: 需要 Metrics Server 获取实际使用量
                    // 当前返回 0，实际应该调用 metrics.k8s.io API
                    return Ok(ResourceStats {
                        cpu: CpuStats {
                            usage_percent: 0.0,
                            usage_cores: 0.0,
                            limit_cores: cpu_limit,
                        },
                        memory: MemoryStats {
                            usage_bytes: 0,
                            usage_percent: 0.0,
                            limit_bytes: memory_limit,
                        },
                        network: NetworkStats {
                            rx_bytes: 0,
                            tx_bytes: 0,
                        },
                        restart_count,
                    });
                }
            }
        }

        warn!("K8s 资源使用查询功能不可用 (app_id: {})", app_id);
        Ok(ResourceStats::default())
    }

    async fn get_app_events(&self, app_id: &str) -> Result<Vec<String>> {
        #[cfg(feature = "kubernetes")]
        {
            if let Some(client) = &self.kube_client {
                let events: Api<Event> = Api::namespaced(client.clone(), &self.config.namespace);

                // 查询与应用相关的事件
                let lp =
                    ListParams::default().fields(&format!("involvedObject.name=app-{}", app_id));

                let event_list = events.list(&lp).await?;

                let event_messages: Vec<String> = event_list
                    .items
                    .iter()
                    .map(|e| {
                        let reason = e.reason.clone().unwrap_or_default();
                        let message = e.message.clone().unwrap_or_default();
                        let timestamp = e
                            .last_timestamp
                            .as_ref()
                            .map(|t| t.0.to_string())
                            .unwrap_or_default();
                        format!("[{}] {}: {}", timestamp, reason, message)
                    })
                    .collect();

                return Ok(event_messages);
            }
        }

        // 非 kubernetes 模式下使用 app_id 记录日志
        warn!("K8s 事件查询功能不可用 (app_id: {})", app_id);
        Ok(vec![])
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

#[cfg(all(test, feature = "kubernetes"))]
mod tests {
    /// 验证 jiff::Timestamp::to_string() 输出的格式与 chrono::DateTime::to_rfc3339() 兼容
    ///
    /// 已知差异：jiff 使用 `Z` 后缀，chrono 使用 `+00:00` 后缀，两者都是有效的 RFC 3339。
    /// 对于 event 日志显示场景，差异无影响。
    #[test]
    fn jiff_timestamp_format_compatible_with_chrono_rfc3339() {
        // 2024-01-15T10:30:00Z = 1705312200 seconds since epoch
        let epoch_secs: i64 = 1705312200;
        let nanos: i32 = 123456789;

        // jiff 格式（通过 k8s_openapi 间接依赖）
        let jiff_ts = k8s_openapi::jiff::Timestamp::new(epoch_secs, nanos).unwrap();
        let jiff_str = jiff_ts.to_string();

        // chrono 格式
        let chrono_dt = chrono::DateTime::from_timestamp(epoch_secs, nanos as u32).unwrap();
        let chrono_str = chrono_dt.to_rfc3339();

        // 两者都应包含日期时间和时区信息
        assert!(jiff_str.contains("2024-01-15"), "jiff: {jiff_str}");
        assert!(chrono_str.contains("2024-01-15"), "chrono: {chrono_str}");

        // 验证日期时间部分（秒精度）一致，忽略时区后缀差异
        // jiff:   2024-01-15T09:50:00.123456789Z
        // chrono: 2024-01-15T09:50:00.123456789+00:00
        let jiff_dt_part = jiff_str.trim_end_matches('Z');
        let chrono_dt_part = chrono_str.trim_end_matches("+00:00");
        assert_eq!(
            jiff_dt_part, chrono_dt_part,
            "日期时间部分不一致:\n  jiff:  {jiff_str}\n  chrono: {chrono_str}"
        );

        // 验证都是 UTC 时区表示
        assert!(
            jiff_str.ends_with('Z') || jiff_str.ends_with("+00:00"),
            "jiff 时区格式异常: {jiff_str}"
        );
        assert!(
            chrono_str.ends_with('Z') || chrono_str.ends_with("+00:00"),
            "chrono 时区格式异常: {chrono_str}"
        );

        // 打印完整格式供人工对比
        println!("jiff:   {jiff_str}");
        println!("chrono: {chrono_str}");
    }

    /// 验证零时间戳的格式
    #[test]
    fn jiff_timestamp_epoch_zero() {
        let jiff_ts = k8s_openapi::jiff::Timestamp::new(0, 0).unwrap();
        let jiff_str = jiff_ts.to_string();

        let chrono_dt = chrono::DateTime::from_timestamp(0, 0).unwrap();
        let chrono_str = chrono_dt.to_rfc3339();

        assert!(jiff_str.starts_with("1970-01-01"), "jiff: {jiff_str}");
        assert!(chrono_str.starts_with("1970-01-01"), "chrono: {chrono_str}");

        println!("epoch zero - jiff:   {jiff_str}");
        println!("epoch zero - chrono: {chrono_str}");
    }
}
