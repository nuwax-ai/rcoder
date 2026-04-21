//! Kubernetes runtime implementation
//!
//! This module provides `KubernetesRuntime` that creates pods in Kubernetes
//! instead of Docker containers, enabling rcoder to work in K8s environments.

#[cfg(feature = "kubernetes")]
use async_trait::async_trait;
#[cfg(feature = "kubernetes")]
use chrono::Utc;
#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    ContainerRuntime, ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus,
    RuntimeContainerInfo,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    Container as K8sContainer, ContainerPort, EnvVar, Pod, PodSpec, Probe,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
#[cfg(feature = "kubernetes")]
use kube::Config;
#[cfg(feature = "kubernetes")]
use kube::api::{Api, DeleteParams, ListParams, ObjectMeta, PostParams};
#[cfg(feature = "kubernetes")]
use kube::client::Client;
#[cfg(feature = "kubernetes")]
use shared_types::{ContainerBasicInfo, ServiceResourceLimits, ServiceType};
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;
#[cfg(feature = "kubernetes")]
use std::sync::Arc;
#[cfg(feature = "kubernetes")]
use tokio::sync::RwLock;
#[cfg(feature = "kubernetes")]
use tracing::{debug, info, warn};

#[cfg(feature = "kubernetes")]
use crate::types::DockerManagerConfig;
#[cfg(feature = "kubernetes")]
const RUNTIME_MANAGED_LABEL: &str = "managed-by=rcoder-runtime";

/// Kubernetes runtime implementation using kube-rs
#[cfg(feature = "kubernetes")]
pub struct KubernetesRuntime {
    client: Client,
    namespace: String,
    config: KubernetesRuntimeConfig,
    /// Cache for pod information (using RwLock to avoid DashMap deadlocks)
    pod_cache: Arc<RwLock<std::collections::HashMap<String, RuntimeContainerInfo>>>,
}

#[cfg(feature = "kubernetes")]
#[derive(Debug, Clone)]
pub struct KubernetesRuntimeConfig {
    /// Namespace where pods are created
    pub namespace: String,
    /// Pod cleanup TTL in seconds
    pub pod_ttl_seconds: Option<u64>,
    /// Default image pull secret (if needed)
    pub image_pull_secret: Option<String>,
    /// Service account name for pods
    pub service_account_name: String,
    /// DockerManagerConfig for image selection (包含 multi_image_config)
    pub docker_manager_config: DockerManagerConfig,
}

#[cfg(feature = "kubernetes")]
impl KubernetesRuntime {
    /// Create a new Kubernetes runtime
    pub async fn new(config: DockerManagerConfig) -> ContainerRuntimeResult<Self> {
        // Load kube config from environment or in-cluster config
        let kube_config = Config::infer().await.map_err(|e| {
            ContainerRuntimeError::K8sError(format!("Failed to load kube config: {}", e))
        })?;

        let client = Client::try_from(kube_config).map_err(|e| {
            ContainerRuntimeError::K8sError(format!("Failed to create K8s client: {}", e))
        })?;

        let namespace =
            std::env::var("RCODER_K8S_NAMESPACE").unwrap_or_else(|_| "default".to_string());

        info!(
            "[K8S] Kubernetes runtime initialized, namespace: {}",
            namespace
        );

        Ok(Self {
            client,
            namespace: namespace.clone(),
            config: KubernetesRuntimeConfig {
                namespace: namespace.clone(),
                pod_ttl_seconds: config.container_ttl_seconds,
                image_pull_secret: None,
                service_account_name: "rcoder-pods-sa".to_string(),
                docker_manager_config: config,
            },
            pod_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    /// Get the Pod API
    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Generate pod name from project_id
    /// K8s Pod names must conform to RFC 1123: lowercase alphanumeric + '-', must start/end with alphanumeric
    /// Replace underscores with hyphens to ensure compatibility
    fn pod_name(&self, project_id: &str, service_type: &ServiceType) -> String {
        let sanitized_id = project_id.replace('_', "-");
        format!("{}-{}", service_type.container_prefix(), sanitized_id)
    }

    /// Extract pod status from Pod object
    fn extract_pod_status(pod: &Pod) -> ContainerRuntimeStatus {
        match &pod.status {
            Some(status) => match status.phase.as_deref() {
                Some("Running") => ContainerRuntimeStatus::Running,
                Some("Succeeded") => ContainerRuntimeStatus::Succeeded,
                Some("Failed") => ContainerRuntimeStatus::Failed,
                Some("Pending") => ContainerRuntimeStatus::Pending,
                Some(phase) => ContainerRuntimeStatus::Unknown(phase.to_string()),
                None => ContainerRuntimeStatus::Unknown("No phase".to_string()),
            },
            None => ContainerRuntimeStatus::Pending,
        }
    }

    fn runtime_info_from_pod(pod: &Pod) -> RuntimeContainerInfo {
        let status = Self::extract_pod_status(pod);
        let metadata = &pod.metadata;
        RuntimeContainerInfo {
            container_id: metadata.uid.clone().unwrap_or_default(),
            container_name: metadata.name.clone().unwrap_or_default(),
            container_ip: pod
                .status
                .as_ref()
                .and_then(|s| s.pod_ip.clone())
                .unwrap_or_default(),
            status,
            created_at: metadata
                .creation_timestamp
                .as_ref()
                .map(|ts| ts.0)
                .unwrap_or_else(Utc::now),
        }
    }

    /// Wait for pod to be ready
    /// 使用 readinessProbe 检查 Pod 是否真正就绪，而非仅检查 Running 状态
    async fn wait_for_pod_ready(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let timeout = std::time::Duration::from_secs(120);
        let start = std::time::Instant::now();
        let pod_name = self.pod_name(identifier, service_type);

        while start.elapsed() < timeout {
            match self.pods().get(&pod_name).await {
                Ok(pod) => {
                    // 检查 Pod phase，提前识别永久失败状态
                    if let Some(phase) = pod.status.as_ref().and_then(|s| s.phase.as_deref()) {
                        match phase {
                            "Failed" | "Succeeded" => {
                                return Err(ContainerRuntimeError::K8sError(format!(
                                    "Pod {} entered terminal state: {}",
                                    pod_name, phase
                                )));
                            }
                            "Running" => {
                                // Pod 运行中，检查 Ready condition
                            }
                            _ => {
                                // Pending 等其他状态，继续等待
                            }
                        }
                    }

                    // 检查 Pod 是否 Ready (需要 readinessProbe 返回成功)
                    if let Some(status) = &pod.status {
                        if let Some(conditions) = &status.conditions {
                            let all_ready = conditions
                                .iter()
                                .any(|c| c.type_ == "Ready" && c.status == "True");
                            if all_ready {
                                info!("[K8S] Pod {} is Ready", pod_name);
                                return Ok(());
                            }
                            // 调试用：打印当前状态
                            let ready_status = conditions
                                .iter()
                                .map(|c| format!("{}={}", c.type_, c.status))
                                .collect::<Vec<_>>()
                                .join(", ");
                            debug!("[K8S] Pod {} conditions: {}", pod_name, ready_status);
                        }
                    }
                }
                Err(kube::Error::Api(ae)) if ae.code == 404 => {
                    // Pod 还没创建，继续等待
                }
                Err(e) => {
                    return Err(ContainerRuntimeError::K8sError(format!(
                        "Failed to get pod '{}': {}",
                        pod_name, e
                    )));
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Err(ContainerRuntimeError::Timeout(
            "Pod did not become ready in time".to_string(),
        ))
    }

    /// Select image based on service type, using multi_image_config from ConfigMap
    fn select_image(&self, service_type: &ServiceType) -> String {
        // 优先使用环境变量（允许运行时覆盖）
        if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE") {
            if !env_image.is_empty() {
                info!("[K8S] Using image from RCODER_DOCKER_IMAGE env: {}", env_image);
                return env_image;
            }
        }
        if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE_COMPUTER") {
            if !env_image.is_empty() {
                info!("[K8S] Using image from RCODER_DOCKER_IMAGE_COMPUTER env: {}", env_image);
                return env_image;
            }
        }

        // 使用 multi_image_config 配置
        let multi_config = &self.config.docker_manager_config.multi_image_config;
        let service_key = service_type.to_string();

        if let Some(service_config) = multi_config.services.get(&service_key) {
            // 优先使用 image 字段
            if let Some(ref image) = service_config.image {
                info!("[K8S] Using image from multi_image_config: {}", image);
                return image.clone();
            }
            // 使用架构特定镜像
            let arch = std::env::consts::ARCH;
            let image = if arch == "aarch64" || arch == "arm64" {
                service_config.arm64_image.clone()
            } else {
                service_config.amd64_image.clone()
            };
            if let Some(img) = image {
                info!("[K8S] Using architecture-specific image: {}", img);
                return img.to_string();
            }
            // 使用默认镜像
            if let Some(ref img) = service_config.default_image {
                info!("[K8S] Using default image: {}", img);
                return img.clone();
            }
        }

        // 兜底：使用硬编码默认值（不应该到达这里，因为 multi_image_config 总是有默认值）
        warn!("[K8S] No image config found, using hardcoded fallback");
        match service_type {
            ServiceType::RCoder => "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder:latest".to_string(),
            ServiceType::ComputerAgentRunner => {
                "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest".to_string()
            }
        }
    }

    /// Build container basic info from runtime container info
    async fn build_container_basic_info(
        &self,
        project_id: &str,
        pod_info: &RuntimeContainerInfo,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        Ok(ContainerBasicInfo {
            container_id: pod_info.container_id.clone(),
            container_name: pod_info.container_name.clone(),
            container_ip: pod_info.container_ip.clone(),
            internal_port: shared_types::HTTP_DEFAULT_PORT,
            external_port: 0,
            project_id: project_id.to_string(),
            status: String::from(pod_info.status.clone()),
            created_at: pod_info.created_at,
            service_url: format!(
                "http://{}:{}",
                pod_info.container_ip,
                shared_types::HTTP_DEFAULT_PORT
            ),
        })
    }
}

#[cfg(feature = "kubernetes")]
#[async_trait]
impl ContainerRuntime for KubernetesRuntime {
    async fn create_container(
        &self,
        project_id: Option<&str>,
        user_id: Option<&str>,
        _host_workspace_path: &str,
        service_type: ServiceType,
        _resource_limits: Option<ServiceResourceLimits>,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        // 确定容器标识符：user_id 优先（ComputerAgentRunner），否则用 project_id
        let identifier = user_id.or(project_id).ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "Either project_id or user_id must be provided".to_string(),
            )
        })?;

        // Pod 名称：当 user_id 存在时使用它（ComputerAgentRunner）来保证唯一性
        let pod_name = match user_id {
            Some(uid) => format!("{}-{}", service_type.container_prefix(), uid),
            None => format!(
                "{}-{}",
                service_type.container_prefix(),
                project_id.unwrap()
            ),
        };

        // Check if pod already exists and is running
        if let Some(cached) = self.pod_cache.read().await.get(identifier) {
            if cached.status == ContainerRuntimeStatus::Running {
                info!("[K8S] Pod {} already exists and is running", pod_name);
                return self
                    .get_container_info_by_identifier(identifier, &service_type)
                    .await?
                    .ok_or_else(|| {
                        ContainerRuntimeError::ContainerNotFound(identifier.to_string())
                    });
            }
        }

        let service_type_str = service_type.to_string();
        let image = self.select_image(&service_type);

        // Build labels using BTreeMap (required by k8s-openapi)
        // 当 user_id 存在时添加 user_id label，用于查询
        let mut label_pairs = vec![
            ("app".to_string(), "rcoder".to_string()),
            ("managed-by".to_string(), "rcoder-runtime".to_string()),
            ("service_type".to_string(), service_type_str.clone()),
        ];
        if let Some(uid) = user_id {
            label_pairs.push(("user_id".to_string(), uid.to_string()));
        } else {
            label_pairs.push(("project_id".to_string(), project_id.unwrap().to_string()));
        }
        let labels: BTreeMap<String, String> = label_pairs.into_iter().collect();

        // Build Pod object using k8s-openapi types
        let pod: Pod = Pod {
            metadata: ObjectMeta {
                name: Some(pod_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                containers: vec![K8sContainer {
                    name: "agent".to_string(),
                    image: Some(image),
                    image_pull_policy: Some("Always".to_string()),
                    env: Some(vec![
                        EnvVar {
                            name: "PROJECT_ID".to_string(),
                            value: Some(project_id.unwrap_or_default().to_string()),
                            ..Default::default()
                        },
                        EnvVar {
                            name: "USER_ID".to_string(),
                            value: Some(user_id.unwrap_or_default().to_string()),
                            ..Default::default()
                        },
                        EnvVar {
                            name: "SERVICE_TYPE".to_string(),
                            value: Some(service_type_str.clone()),
                            ..Default::default()
                        },
                    ]),
                    ports: Some(vec![
                        ContainerPort {
                            container_port: shared_types::GRPC_DEFAULT_PORT as i32,
                            name: Some("grpc".to_string()),
                            ..Default::default()
                        },
                        // HTTP health check port for agent_runner
                        ContainerPort {
                            container_port: 8086,
                            name: Some("http".to_string()),
                            ..Default::default()
                        },
                    ]),
                    readiness_probe: Some(Probe {
                        http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                            path: Some("/health".to_string()),
                            port: IntOrString::Int(8086),
                            ..Default::default()
                        }),
                        initial_delay_seconds: Some(3),
                        period_seconds: Some(3),
                        timeout_seconds: Some(3),
                        failure_threshold: Some(20),
                        success_threshold: Some(1),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                restart_policy: Some("Never".to_string()),
                service_account_name: Some(self.config.service_account_name.clone()),
                ..Default::default()
            }),
            status: None,
        };

        let pp = PostParams::default();
        self.pods().create(&pp, &pod).await.map_err(|e| {
            ContainerRuntimeError::ContainerCreationError(format!("Failed to create pod: {}", e))
        })?;

        info!("[K8S] Pod {} created successfully", pod_name);

        // Wait for pod to be ready
        self.wait_for_pod_ready(identifier, &service_type).await?;

        // Get pod info
        self.get_container_info_by_identifier(identifier, &service_type)
            .await?
            .ok_or_else(|| {
                ContainerRuntimeError::ContainerCreationError(
                    "Pod created but info not found".to_string(),
                )
            })
    }

    async fn get_container_info(
        &self,
        identifier: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        // Try cache first
        if let Some(cached) = self.pod_cache.read().await.get(identifier) {
            if cached.status == ContainerRuntimeStatus::Running {
                return Ok(Some(
                    self.build_container_basic_info(identifier, cached).await?,
                ));
            }
        }

        // Query K8s API - 尝试两种稳定标识查询：
        // 1. project_id label
        // 2. user_id label
        let search_queries = vec![
            format!("project_id={}", identifier),
            format!("user_id={}", identifier),
        ];

        for query in search_queries {
            let lp = ListParams::default().labels(&query);
            if let Ok(pods) = self.pods().list(&lp).await {
                for p in pods.items {
                    let pod: Pod = p;
                    let status = Self::extract_pod_status(&pod);
                    let metadata = &pod.metadata;
                    let uid = metadata.uid.clone().unwrap_or_default();
                    let name = metadata.name.clone().unwrap_or_default();
                    let pod_ip = pod
                        .status
                        .as_ref()
                        .and_then(|s| s.pod_ip.clone())
                        .unwrap_or_default();
                    let created_at = metadata
                        .creation_timestamp
                        .as_ref()
                        .map(|ts| ts.0)
                        .unwrap_or_else(|| Utc::now());

                    let pod_info = RuntimeContainerInfo {
                        container_id: uid,
                        container_name: name,
                        container_ip: pod_ip,
                        status,
                        created_at,
                    };

                    // Update cache if running
                    if pod_info.status == ContainerRuntimeStatus::Running {
                        self.pod_cache
                            .write()
                            .await
                            .insert(identifier.to_string(), pod_info.clone());
                    }

                    return Ok(Some(
                        self.build_container_basic_info(identifier, &pod_info)
                            .await?,
                    ));
                }
            }
        }

        Ok(None)
    }

    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        _service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.get_container_info(identifier).await
    }

    async fn find_container(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        // Check cache first
        if let Some(cached) = self.pod_cache.read().await.get(identifier) {
            return Ok(Some(cached.clone()));
        }

        // 1) Query by concrete pod name
        let pod_name = self.pod_name(identifier, service_type);
        match self.pods().get(&pod_name).await {
            Ok(pod) => return Ok(Some(Self::runtime_info_from_pod(&pod))),
            Err(kube::Error::Api(ae)) if ae.code == 404 => {}
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Failed to get pod by name '{}': {}",
                    pod_name, e
                )));
            }
        }

        // 2) Query by labels
        for selector in [
            format!("user_id={}", identifier),
            format!("project_id={}", identifier),
        ] {
            let pods = self
                .pods()
                .list(&ListParams::default().labels(&selector).limit(1))
                .await
                .map_err(|e| {
                    ContainerRuntimeError::K8sError(format!(
                        "Failed to list pods with selector '{}': {}",
                        selector, e
                    ))
                })?;

            if let Some(pod) = pods.items.into_iter().next() {
                return Ok(Some(Self::runtime_info_from_pod(&pod)));
            }
        }

        Ok(None)
    }

    async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()> {
        // First check if pod exists with either service type to avoid unnecessary 404
        // Try both service types - one of them should have the pod
        let rcoder_exists = self
            .find_container(project_id, &ServiceType::RCoder)
            .await?
            .is_some();
        let computer_exists = self
            .find_container(project_id, &ServiceType::ComputerAgentRunner)
            .await?
            .is_some();

        if rcoder_exists {
            self.stop_container_by_identifier(project_id, &ServiceType::RCoder)
                .await?;
            info!(
                "[K8S] Pod for project {} deleted successfully (RCoder)",
                project_id
            );
            return Ok(());
        }

        if computer_exists {
            self.stop_container_by_identifier(project_id, &ServiceType::ComputerAgentRunner)
                .await?;
            info!(
                "[K8S] Pod for project {} deleted successfully (ComputerAgentRunner)",
                project_id
            );
            return Ok(());
        }

        // Pod doesn't exist - this is OK, consider it already stopped
        Ok(())
    }

    async fn stop_container_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let pod_name = self.pod_name(identifier, service_type);

        match self
            .pods()
            .delete(&pod_name, &DeleteParams::default())
            .await
        {
            Ok(_) => {
                self.pod_cache.write().await.remove(identifier);
                info!("[K8S] Pod {} deleted successfully", pod_name);
                Ok(())
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
            Err(e) => Err(ContainerRuntimeError::ContainerStopError(format!(
                "Failed to delete pod {}: {}",
                pod_name, e
            ))),
        }
    }

    async fn is_container_running(&self, project_id: &str) -> ContainerRuntimeResult<bool> {
        Ok(self
            .find_container(project_id, &ServiceType::RCoder)
            .await?
            .map(|p| p.status == ContainerRuntimeStatus::Running)
            .unwrap_or(false))
    }

    async fn is_container_running_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self
            .find_container(identifier, service_type)
            .await?
            .map(|p| p.status == ContainerRuntimeStatus::Running)
            .unwrap_or(false))
    }

    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        let lp = ListParams::default().labels(RUNTIME_MANAGED_LABEL);
        let pods =
            self.pods().list(&lp).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("Failed to list pods: {}", e))
            })?;

        let mut result = Vec::new();
        for p in pods.items {
            let pod: Pod = p;
            let status = Self::extract_pod_status(&pod);
            let metadata = &pod.metadata;
            let pod_info = RuntimeContainerInfo {
                container_id: metadata.uid.clone().unwrap_or_default(),
                container_name: metadata.name.clone().unwrap_or_default(),
                container_ip: pod
                    .status
                    .as_ref()
                    .and_then(|s| s.pod_ip.clone())
                    .unwrap_or_default(),
                status,
                created_at: metadata
                    .creation_timestamp
                    .as_ref()
                    .map(|ts| ts.0)
                    .unwrap_or_else(|| Utc::now()),
            };
            result.push(pod_info);
        }

        Ok(result)
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        let lp = ListParams::default().labels(RUNTIME_MANAGED_LABEL);
        let _ = self
            .pods()
            .delete_collection(&DeleteParams::default(), &lp)
            .await
            .map_err(|e| {
                ContainerRuntimeError::ConnectionError(format!("Failed to cleanup pods: {}", e))
            })?;

        self.pod_cache.write().await.clear();
        Ok(())
    }

    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        // Try to list pods as a health check
        let lp = ListParams::default().limit(1);
        self.pods().list(&lp).await.map_err(|e| {
            ContainerRuntimeError::ConnectionError(format!("K8s health check failed: {}", e))
        })?;
        Ok(())
    }
}
