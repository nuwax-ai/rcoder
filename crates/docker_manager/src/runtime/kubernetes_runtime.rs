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
    ContainerCreateParams, ContainerRuntime, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerRuntimeStatus, RuntimeContainerInfo,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    Container as K8sContainer, ContainerPort, EnvVar,
    LocalObjectReference, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    Pod, PodSecurityContext, PodSpec, Probe, ResourceRequirements,
    Volume, VolumeMount, VolumeResourceRequirements,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
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
    /// NFS Server address (K8s DNS 或外部 IP)
    pub nfs_server: String,
    /// NFS 共享路径
    pub nfs_path: String,
    /// StorageClass 名称 (nfs-subdir-external-provisioner 创建的 SC)
    pub storage_class: String,
    /// PVC 访问模式: ReadWriteMany (默认, JuiceFS/NFS) 或 ReadWriteOnce (local-path)
    pub access_mode: String,
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

        // NFS 存储配置 (支持外部 NFS Server)
        let nfs_server = std::env::var("RCODER_K8S_NFS_SERVER")
            .unwrap_or_else(|_| "nfs-server.nfs-storage.svc.cluster.local".to_string());
        let nfs_path = std::env::var("RCODER_K8S_NFS_PATH")
            .unwrap_or_else(|_| "/exports".to_string());
        let storage_class = std::env::var("RCODER_K8S_STORAGE_CLASS")
            .unwrap_or_else(|_| "rcoder-nfs".to_string());
        let access_mode = std::env::var("RCODER_K8S_PVC_ACCESS_MODE")
            .unwrap_or_else(|_| "ReadWriteMany".to_string());

        info!(
            "[K8S] Kubernetes runtime initialized, namespace: {}",
            namespace
        );
        info!("[K8S] NFS storage: server={}, path={}, storage_class={}, access_mode={}",
            nfs_server, nfs_path, storage_class, access_mode);

        Ok(Self {
            client,
            namespace: namespace.clone(),
            config: KubernetesRuntimeConfig {
                namespace: namespace.clone(),
                pod_ttl_seconds: config.container_ttl_seconds,
                image_pull_secret: std::env::var("RCODER_K8S_IMAGE_PULL_SECRET").ok(),
                service_account_name: "rcoder-pods-sa".to_string(),
                nfs_server,
                nfs_path,
                storage_class,
                access_mode,
                docker_manager_config: config,
            },
            pod_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    /// Get the Pod API
    fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Get the PVC API
    fn pvcs(&self) -> Api<PersistentVolumeClaim> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Get workspace PVC name for a project/user
    /// PVC 名称包含 identifier 以实现存储隔离
    fn workspace_pvc_name(identifier: &str, service_type: &ServiceType) -> String {
        let sanitized = identifier.replace('_', "-");
        format!("{}-{}-workspace", service_type.container_prefix(), sanitized)
    }

    /// Ensure workspace PVC exists, create if not
    async fn ensure_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        storage_size: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        let pvc_name = Self::workspace_pvc_name(identifier, service_type);

        // Check if PVC already exists
        let pvc_exists = match self.pvcs().get(&pvc_name).await {
            Ok(_) => {
                info!("[K8S] PVC {} already exists", pvc_name);
                true
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => false,
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Failed to check PVC '{}': {}",
                    pvc_name, e
                )));
            }
        };

        // If PVC already exists, return immediately
        // WaitForFirstConsumer PVCs will be Bound once a Pod referencing them is scheduled
        if pvc_exists {
            info!("[K8S] PVC {} already exists, skipping Bound check (WaitForFirstConsumer)", pvc_name);
            return Ok(());
        }
        // If not found, create it (falls through to creation logic below)

        let storage_size = storage_size.unwrap_or("10Gi");

        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(pvc_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some({
                    let mut m = BTreeMap::new();
                    m.insert("app".to_string(), "rcoder".to_string());
                    m.insert("managed-by".to_string(), "rcoder-runtime".to_string());
                    m.insert("service_type".to_string(), service_type.to_string());
                    m
                }),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                access_modes: Some(vec![self.config.access_mode.clone()]),
                storage_class_name: Some(self.config.storage_class.clone()),
                resources: Some(VolumeResourceRequirements {
                    requests: Some({
                        let mut r = BTreeMap::new();
                        r.insert(
                            "storage".to_string(),
                            Quantity(format!("{}", storage_size)),
                        );
                        r
                    }),
                    ..Default::default()
                }),
                volume_name: None,
                ..Default::default()
            }),
            status: None,
        };

        self.pvcs().create(&PostParams::default(), &pvc).await.map_err(|e| {
            ContainerRuntimeError::ContainerCreationError(format!(
                "Failed to create PVC '{}': {}",
                pvc_name, e
            ))
        })?;

        info!("[K8S] PVC {} created", pvc_name);

        // 不等待 PVC Bound：WaitForFirstConsumer 存储类需要 Pod 调度后才会绑定 PVC
        // 直接返回，让后续 Pod 创建触发 PVC 绑定
        Ok(())
    }

    /// Wait for PVC to be in Bound state
    async fn wait_for_pvc_bound(&self, pvc_name: &str) -> ContainerRuntimeResult<()> {
        let wait_timeout = std::time::Duration::from_secs(60);
        let start = std::time::Instant::now();
        while start.elapsed() < wait_timeout {
            match self.pvcs().get(pvc_name).await {
                Ok(pvc) => {
                    if pvc.status.as_ref()
                        .and_then(|s| s.phase.as_deref())
                        == Some("Bound")
                    {
                        return Ok(());
                    }
                    debug!(
                        "[K8S] PVC {} phase: {:?}",
                        pvc_name,
                        pvc.status.as_ref().and_then(|s| s.phase.clone())
                    );
                }
                Err(kube::Error::Api(ae)) if ae.code == 404 => {}
                Err(e) => {
                    warn!("[K8S] Failed to check PVC '{}' status: {}", pvc_name, e);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        Err(ContainerRuntimeError::Timeout(format!(
            "PVC '{}' did not become Bound in time",
            pvc_name
        )))
    }

    /// Delete workspace PVC for a project/user
    async fn delete_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let pvc_name = Self::workspace_pvc_name(identifier, service_type);

        match self.pvcs().delete(&pvc_name, &DeleteParams::default()).await {
            Ok(_) => {
                info!("[K8S] PVC {} deleted successfully", pvc_name);
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                debug!("[K8S] PVC {} not found, skip delete", pvc_name);
            }
            Err(e) => {
                warn!("[K8S] Failed to delete PVC '{}': {}", pvc_name, e);
            }
        }
        Ok(())
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

    /// Detect pod failure reason from container statuses
    fn detect_container_failure(pod: &Pod) -> Option<String> {
        let statuses = pod.status.as_ref()?.container_statuses.as_ref()?;
        for cs in statuses {
            if let Some(state) = &cs.state {
                if let Some(waiting) = &state.waiting {
                    if let Some(reason) = &waiting.reason {
                        return Some(reason.clone());
                    }
                }
            }
        }
        None
    }

    /// Wait for pod to be ready
    /// 使用 readinessProbe 检查 Pod 是否真正就绪，而非仅检查 Running 状态
    async fn wait_for_pod_ready(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        // Pod wait timeout: configurable from config, default 120s
        let timeout = std::time::Duration::from_secs(
            self.config
                .pod_ttl_seconds
                .unwrap_or(120),
        );
        let start = std::time::Instant::now();
        let pod_name = self.pod_name(identifier, service_type);

        while start.elapsed() < timeout {
            match self.pods().get(&pod_name).await {
                Ok(pod) => {
                    // 检查 Pod phase，提前识别永久失败状态
                    if let Some(phase) = pod.status.as_ref().and_then(|s| s.phase.as_deref()) {
                        match phase {
                            "Failed" => {
                                // Pod 实际失败：容器异常退出/OOM/镜像拉失败 等
                                let reason = Self::detect_container_failure(&pod);
                                return Err(ContainerRuntimeError::K8sError(format!(
                                    "Pod {} entered terminal state: Failed, reason: {:?}",
                                    pod_name, reason
                                )));
                            }
                            "Succeeded" => {
                                // 容器跑完正常退出（run-to-completion 场景，例如一次性任务型 Pod）。
                                // 这不是失败：pod 已成功执行到结束。对调用方而言"ready" 的语义是
                                // "容器至少跑起来过"，Succeeded 满足这个语义。
                                info!(
                                    "[K8S] Pod {} completed with phase=Succeeded (run-to-completion)",
                                    pod_name
                                );
                                return Ok(());
                            }
                            "Running" => {
                                // Pod 运行中，检查 Ready condition
                            }
                            _ => {
                                // Pending 等其他状态，继续等待
                            }
                        }
                    }

                    // 检测 CrashLoopBackOff 和 ImagePullBackOff
                    if let Some(reason) = Self::detect_container_failure(&pod) {
                        match reason.as_str() {
                            "CrashLoopBackOff" => {
                                return Err(ContainerRuntimeError::K8sError(format!(
                                    "Pod {} is in CrashLoopBackOff state",
                                    pod_name
                                )));
                            }
                            "ImagePullBackOff" => {
                                return Err(ContainerRuntimeError::K8sError(format!(
                                    "Pod {} failed to pull image: {:?}",
                                    pod_name,
                                    pod.status.as_ref().and_then(|s| s.container_statuses.as_ref())
                                )));
                            }
                            _ => {
                                // 其他 waiting 状态，继续等待
                                debug!("[K8S] Pod {} waiting: {}", pod_name, reason);
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
        // 注意：ComputerAgentRunner 必须优先检查 RCODER_DOCKER_IMAGE_COMPUTER
        match service_type {
            ServiceType::ComputerAgentRunner => {
                if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE_COMPUTER") {
                    if !env_image.is_empty() {
                        info!("[K8S] Using image from RCODER_DOCKER_IMAGE_COMPUTER env: {}", env_image);
                        return env_image;
                    }
                }
                if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE") {
                    if !env_image.is_empty() {
                        info!("[K8S] Using image from RCODER_DOCKER_IMAGE env: {}", env_image);
                        return env_image;
                    }
                }
            }
            _ => {
                if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE") {
                    if !env_image.is_empty() {
                        info!("[K8S] Using image from RCODER_DOCKER_IMAGE env: {}", env_image);
                        return env_image;
                    }
                }
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

    /// Build resource requirements for K8s container from ServiceResourceLimits
    fn build_resource_requirements(
        limits: &ServiceResourceLimits,
    ) -> Option<ResourceRequirements> {
        let mut requests: std::collections::BTreeMap<String, Quantity> =
            std::collections::BTreeMap::new();
        let mut lims: std::collections::BTreeMap<String, Quantity> =
            std::collections::BTreeMap::new();

        if let Some(memory) = limits.memory_limit {
            // memory_limit is in bytes, convert to Mi
            let mem_mb = (memory / (1024.0 * 1024.0)) as i64;
            // Quantity is a string wrapper, construct directly with formatted string
            requests.insert(
                "memory".to_string(),
                Quantity(format!("{}Mi", mem_mb)),
            );
            lims.insert("memory".to_string(), Quantity(format!("{}Mi", mem_mb)));
        }
        if let Some(cpu) = limits.cpu_limit {
            // cpu_limit is core count, format as decimal string
            requests.insert(
                "cpu".to_string(),
                Quantity(format!("{}", cpu)),
            );
            lims.insert("cpu".to_string(), Quantity(format!("{}", cpu)));
        }

        if requests.is_empty() && lims.is_empty() {
            return None;
        }
        Some(ResourceRequirements {
            claims: None,
            requests: Some(requests),
            limits: Some(lims),
        })
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
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        let ContainerCreateParams {
            project_id,
            user_id,
            host_workspace_path: _,
            service_type,
            resource_limits,
            pod_id,
            isolation_type,
            tenant_id,
            space_id,
        } = params;

        // 确定容器标识符：pod_id > user_id > project_id（与 Docker 模式一致）
        let project_id_val = project_id.clone().unwrap_or_default();
        let user_id_val = user_id.clone().unwrap_or_default();
        let identifier = pod_id.as_ref()
            .or(user_id.as_ref())
            .or(project_id.as_ref())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "At least one of pod_id, user_id, or project_id must be provided".to_string(),
                )
            })?;

        // Pod 名称：统一使用 pod_name() helper（含 RFC 1123 下划线清理）
        let pod_name = self.pod_name(identifier, &service_type);

        // Ensure workspace PVC exists first (NFS-backed, each project/user gets its own PVC)
        // The PVC is backed by NFS Subdir External Provisioner which automatically
        // creates NFS subdirectory per PVC for isolation and automatic cleanup
        // Note: ensure_workspace_pvc waits for PVC Bound state before returning
        self.ensure_workspace_pvc(identifier, &service_type, None).await?;

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
        // Label 与 identifier 优先级一致：pod_id > user_id > project_id
        let mut label_pairs = vec![
            ("app".to_string(), "rcoder".to_string()),
            ("managed-by".to_string(), "rcoder-runtime".to_string()),
            ("service_type".to_string(), service_type_str.clone()),
        ];
        if pod_id.is_some() {
            label_pairs.push(("pod_id".to_string(), identifier.clone()));
        } else if user_id.is_some() {
            label_pairs.push(("user_id".to_string(), identifier.clone()));
        } else {
            label_pairs.push(("project_id".to_string(), identifier.clone()));
        }
        let labels: BTreeMap<String, String> = label_pairs.into_iter().collect();

        // Build Pod object using k8s-openapi types
        // Note: Pod existence is already checked via cache above.
        // The API-level check (for race conditions) is intentionally omitted here
        // to avoid extra K8s API call overhead. If create() fails with 409 Conflict,
        // the error will propagate and the caller should handle it.

        // Build resource requirements if limits are provided
        let resources = resource_limits
            .as_ref()
            .and_then(|limits| Self::build_resource_requirements(limits));

        // Build workspace volume using PVC (NFS-backed persistent storage)
        // 每个项目/用户使用独立的 PVC，底层由 NFS Subdir External Provisioner
        // 自动在 NFS Server 上创建子目录，实现存储隔离和自动回收
        let pvc_name = Self::workspace_pvc_name(identifier, &service_type);
        let volumes = Some(vec![Volume {
            name: "workspace".to_string(),
            persistent_volume_claim: Some(
                k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                    claim_name: pvc_name.clone(),
                    read_only: Some(false),
                },
            ),
            ..Default::default()
        }]);
        let volume_mounts = Some(vec![VolumeMount {
            name: "workspace".to_string(),
            mount_path: "/app/project_workspace".to_string(),
            read_only: Some(false),
            ..Default::default()
        }]);

        // Build image pull secrets if configured
        let image_pull_secrets = self.config.image_pull_secret.as_ref().map(|secret| {
            vec![LocalObjectReference {
                name: secret.clone(),
            }]
        });

        let pod: Pod = Pod {
            metadata: ObjectMeta {
                name: Some(pod_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(labels),
                ..Default::default()
            },
            spec: Some(PodSpec {
                volumes,
                image_pull_secrets,
                security_context: Some(PodSecurityContext {
                    run_as_non_root: Some(false),
                    ..Default::default()
                }),
                termination_grace_period_seconds: Some(60),
                containers: vec![K8sContainer {
                    name: "agent".to_string(),
                    image: Some(image),
                    // IfNotPresent: 动态 pod 频繁创建（每 chat/computer-chat 一个），
                    // 节点已缓存就直接用，避免每次都去 registry 验 token/manifest。
                    // image 更新由主 Deployment 触发拉取（用户做 rollout restart 时），
                    // 主服务用新 image 启动后，动态 pod 跟着用同样的 image 引用。
                    image_pull_policy: Some("IfNotPresent".to_string()),
                    // 启动命令由 orchestration 层显式指定，避免依赖镜像默认行为：
                    //   - RCoder 服务类型：运行 agent_runner binary（gRPC 50051 + HTTP 8086）。
                    //     注意 rcoder-master 镜像本身没有 CMD/ENTRYPOINT，必须显式指定。
                    //   - ComputerAgentRunner 服务类型：使用镜像自己的 ENTRYPOINT（start-up.sh）。
                    command: match service_type {
                        ServiceType::RCoder => Some(vec!["/app/bin/agent_runner".to_string()]),
                        ServiceType::ComputerAgentRunner => None,
                    },
                    env: {
                        let mut env_vars = vec![
                            EnvVar {
                                name: "PROJECT_ID".to_string(),
                                value: Some(project_id_val.to_string()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "USER_ID".to_string(),
                                value: Some(user_id_val.to_string()),
                                ..Default::default()
                            },
                            EnvVar {
                                name: "SERVICE_TYPE".to_string(),
                                value: Some(service_type_str.clone()),
                                ..Default::default()
                            },
                        ];
                        // 多租户环境变量（agent_runner 用于构建工作目录路径）
                        if let Some(ref tid) = tenant_id {
                            env_vars.push(EnvVar {
                                name: "TENANT_ID".to_string(),
                                value: Some(tid.clone()),
                                ..Default::default()
                            });
                        }
                        if let Some(ref sid) = space_id {
                            env_vars.push(EnvVar {
                                name: "SPACE_ID".to_string(),
                                value: Some(sid.clone()),
                                ..Default::default()
                            });
                        }
                        if let Some(ref it) = isolation_type {
                            env_vars.push(EnvVar {
                                name: "ISOLATION_TYPE".to_string(),
                                value: Some(it.clone()),
                                ..Default::default()
                            });
                        }
                        Some(env_vars)
                    },
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
                    resources,
                    volume_mounts,
                    liveness_probe: Some(Probe {
                        http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                            path: Some("/health".to_string()),
                            port: IntOrString::Int(8086),
                            ..Default::default()
                        }),
                        initial_delay_seconds: Some(30),
                        period_seconds: Some(10),
                        timeout_seconds: Some(3),
                        failure_threshold: Some(3),
                        success_threshold: Some(1),
                        ..Default::default()
                    }),
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

        // Query K8s API - 尝试多种标识查询（与 label 构建逻辑一致）
        let search_queries = vec![
            format!("pod_id={}", identifier),
            format!("user_id={}", identifier),
            format!("project_id={}", identifier),
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
            format!("pod_id={}", identifier),
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
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                info!("[K8S] Pod {} not found, skip delete", pod_name);
            }
            Err(e) => {
                warn!("[K8S] Failed to delete pod {}: {}", pod_name, e);
            }
        }

        // Also delete the workspace PVC (NFS-backed storage)
        // NFS Subdir External Provisioner will automatically delete the NFS subdirectory
        self.delete_workspace_pvc(identifier, service_type).await?;

        Ok(())
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
        // Clean up all managed pods
        let lp = ListParams::default().labels(RUNTIME_MANAGED_LABEL);
        let _ = self
            .pods()
            .delete_collection(&DeleteParams::default(), &lp)
            .await
            .map_err(|e| {
                ContainerRuntimeError::ConnectionError(format!("Failed to cleanup pods: {}", e))
            })?;

        // Clean up all managed PVCs (workspace PVCs for each project/user)
        // These have the managed-by=rcoder-runtime label
        let pvc_lp = ListParams::default().labels(RUNTIME_MANAGED_LABEL);
        let _ = self
            .pvcs()
            .delete_collection(&DeleteParams::default(), &pvc_lp)
            .await
            .map_err(|e| {
                ContainerRuntimeError::ConnectionError(format!("Failed to cleanup PVCs: {}", e))
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
