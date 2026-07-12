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
    ContainerCreateParams, ContainerLogEntry, ContainerRuntime, ContainerRuntimeError,
    ContainerRuntimeResult, ContainerRuntimeStatus, DeploymentStatus, RemovedContainerInfo,
    RuntimeContainerInfo,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    ConfigMapVolumeSource, Container as K8sContainer, ContainerPort, EmptyDirVolumeSource, EnvVar,
    LocalObjectReference, PersistentVolumeClaim, PersistentVolumeClaimVolumeSource, Pod,
    PodSecurityContext, PodSpec, Probe, ResourceRequirements, Service, Volume, VolumeMount,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
#[cfg(feature = "kubernetes")]
use kube::Config;
#[cfg(feature = "kubernetes")]
use kube::api::{Api, DeleteParams, DynamicObject, ListParams, ObjectMeta, PostParams};
#[cfg(feature = "kubernetes")]
use kube::client::Client;
#[cfg(feature = "kubernetes")]
use shared_types::{
    ContainerBasicInfo, K8sSidecarSpec, K8sVolumeMountSpec, K8sVolumeSpec, K8sVolumeType,
    ServiceResourceLimits, ServiceType,
};
#[cfg(feature = "kubernetes")]
use shared_types::paths::{COMPUTER_WORKSPACE_ROOT, WORKSPACE_ROOT};
#[cfg(feature = "kubernetes")]
use std::sync::Arc;
#[cfg(feature = "kubernetes")]
use tokio::sync::RwLock;
#[cfg(feature = "kubernetes")]
use tracing::{info, warn};

#[cfg(feature = "kubernetes")]
use super::{
    k8s_backend_crd::K8sBackendCRDOps,
    k8s_pod::K8sPodOps,
    k8s_pvc::K8sPvcOps,
    k8s_service::{K8sServiceOps, build_standard_labels},
};
#[cfg(feature = "kubernetes")]
use crate::types::DockerManagerConfig;
#[cfg(feature = "kubernetes")]
// 全键：Pod/Service 经 build_standard_labels 写入的是 app.kubernetes.io/managed-by
// （K8s 惯例）。裸 key "managed-by" 只历史性地写在 PVC/Backend CRD 上，
// 会导致 cleanup_all/list_containers 的 label selector 匹配不到 Pod/Service（空跑）。
// 此处与 PVC/Backend CRD 的 label 写入一并对齐到全键。
const RUNTIME_MANAGED_LABEL: &str = "app.kubernetes.io/managed-by=rcoder-runtime";

/// 解析 K8s quantity 字符串为字节数 (`"10Gi"` -> 10737418240, `"500M"` -> 500000000)。
/// 用于 CephFS 目录配额 (ceph.quota.max_bytes 需字节数)。未识别格式返回 None。
#[cfg(feature = "kubernetes")]
fn parse_quantity_to_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 二进制后缀 (Ki/Mi/Gi/Ti/Pi/Ei)
    for (suffix, unit) in [
        ("Ei", 1u64 << 60),
        ("Pi", 1u64 << 50),
        ("Ti", 1u64 << 40),
        ("Gi", 1u64 << 30),
        ("Mi", 1u64 << 20),
        ("Ki", 1u64 << 10),
    ] {
        if let Some(num) = s.strip_suffix(suffix) {
            return num.parse::<u64>().ok().map(|n| n.saturating_mul(unit));
        }
    }
    // 十进制后缀 (E/P/T/G/M/K)
    for (suffix, unit) in [
        ("E", 1_000_000_000_000_000_000u64),
        ("P", 1_000_000_000_000_000),
        ("T", 1_000_000_000_000),
        ("G", 1_000_000_000),
        ("M", 1_000_000),
        ("K", 1_000),
    ] {
        if let Some(num) = s.strip_suffix(suffix) {
            return num.parse::<u64>().ok().map(|n| n.saturating_mul(unit));
        }
    }
    // 纯数字 (字节)
    s.parse::<u64>().ok()
}

/// 计算 rcoder 主容器内 agent 工作区子目录 (用于 CephFS 配额)。
/// 逻辑与 `rcoder::handler::utils::paths::build_workspace_path` /
/// `build_computer_workspace_path` 保持一致 (单一事实源在那边; 此处复刻是因为
/// docker_manager 不能反向依赖 rcoder crate)。返回的是 **rcoder 主容器视角** 路径
/// (不是 agent sub-container 的 /home/user)。
///
/// - web 共享容器 (isolation=tenant/space): 三级 `{tenant}/{space}/{project}` (per-project)
/// - web 普通隔离 (project): 单级 `{project}`
/// - computer: per-user `{user_id}` (与 subPath=user_id 挂载边界对齐, 限该用户所有 project 总和)
/// - pod_id 不参与路径决策 (只影响容器名/缓存键)
/// - 共享容器下 handler 强制 tenant_id/space_id 非空; 若脏数据缺失则返回 None (放弃配额, 不设到错误目录)
#[cfg(feature = "kubernetes")]
fn agent_workspace_quota_dir(
    service_type: &ServiceType,
    isolation_type: Option<&str>,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    project_id: &str,
    user_id: &str,
) -> Option<String> {
    // 标识符校验防路径穿越 (与 rcoder::handler::utils::paths::build_workspace_path 一致,
    // 复用 shared_types::validation::validate_identifier: 仅 [a-zA-Z0-9_-], 1-64 字符, 拒 . /)。
    // 不通过则放弃配额 (None), 绝不设到可能穿越的错误目录 (如 project_id="../etc")。
    use shared_types::validation::validate_identifier;
    let iso = isolation_type.map(str::to_lowercase);
    match service_type {
        ServiceType::WebAgentRunner => {
            if validate_identifier(project_id, "project_id").is_err() {
                return None;
            }
            match iso.as_deref() {
                Some("tenant") | Some("space") => {
                    let tid = tenant_id?;
                    let sid = space_id?;
                    if validate_identifier(tid, "tenant_id").is_err()
                        || validate_identifier(sid, "space_id").is_err()
                    {
                        return None;
                    }
                    Some(format!("{WORKSPACE_ROOT}/{tid}/{sid}/{project_id}"))
                }
                _ => Some(format!("{WORKSPACE_ROOT}/{project_id}")),
            }
        }
        ServiceType::ComputerAgentRunner => {
            if validate_identifier(user_id, "user_id").is_err() {
                return None;
            }
            Some(format!("{COMPUTER_WORKSPACE_ROOT}/{user_id}"))
        }
        _ => None,
    }
}

/// Kubernetes runtime implementation using kube-rs
#[cfg(feature = "kubernetes")]
pub struct KubernetesRuntime {
    pub(crate) client: Client,
    pub(crate) namespace: String,
    pub(crate) config: KubernetesRuntimeConfig,
    /// Cache for pod information (using RwLock to avoid DashMap deadlocks)
    pub(crate) pod_cache: Arc<RwLock<std::collections::HashMap<String, RuntimeContainerInfo>>>,
}

#[cfg(feature = "kubernetes")]
#[derive(Debug, Clone)]
pub struct KubernetesRuntimeConfig {
    /// Namespace where pods are created
    pub namespace: String,
    /// K8s cluster domain (default: "cluster.local")
    /// 用于构建 K8s Service FQDN: <service>.<namespace>.svc.<cluster_domain>
    pub cluster_domain: String,
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
    /// K8s 运行时专用配置(自包含 image/env/command/卷/sidecar;K8s 构建器只读它)
    pub kubernetes_config: shared_types::KubernetesConfig,
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

        // K8s 集群域名配置 (用于构建 Service FQDN)
        let cluster_domain = shared_types::get_k8s_cluster_domain();

        // NFS 存储配置 (支持外部 NFS Server)
        let nfs_server = std::env::var("RCODER_K8S_NFS_SERVER")
            .unwrap_or_else(|_| "nfs-server.nfs-storage.svc.cluster.local".to_string());
        let nfs_path =
            std::env::var("RCODER_K8S_NFS_PATH").unwrap_or_else(|_| "/exports".to_string());
        let storage_class =
            std::env::var("RCODER_K8S_STORAGE_CLASS").unwrap_or_else(|_| "rcoder-nfs".to_string());
        let access_mode = std::env::var("RCODER_K8S_PVC_ACCESS_MODE")
            .unwrap_or_else(|_| "ReadWriteMany".to_string());

        info!(
            "[K8S] Kubernetes runtime initialized, namespace: {}, cluster_domain: {}",
            namespace, cluster_domain
        );
        info!(
            "[K8S] NFS storage: server={}, path={}, storage_class={}, access_mode={}",
            nfs_server, nfs_path, storage_class, access_mode
        );

        // 先取出 kubernetes_config(克隆),之后把 config 整体 move 进 docker_manager_config,
        // 避免克隆整个 DockerManagerConfig(含 multi_image_config 的 HashMap)。
        let kubernetes_config = config.kubernetes_config.clone();
        // pod_ttl_seconds 是 Copy,move 前读取即可。
        let pod_ttl_seconds = config.container_ttl_seconds;

        Ok(Self {
            client,
            namespace: namespace.clone(),
            config: KubernetesRuntimeConfig {
                namespace: namespace.clone(),
                cluster_domain,
                pod_ttl_seconds,
                image_pull_secret: std::env::var("RCODER_K8S_IMAGE_PULL_SECRET").ok(),
                // agent-runner Pod 的 ServiceAccount 名（helm 注入 RCODER_AGENT_RUNNER_SA）。
                // 兜底 rcoder-pods-sa 以兼容未注入该 env 的旧 chart，不破现有部署。
                service_account_name: std::env::var("RCODER_AGENT_RUNNER_SA")
                    .unwrap_or_else(|_| "rcoder-pods-sa".to_string()),
                nfs_server,
                nfs_path,
                storage_class,
                access_mode,
                docker_manager_config: config,
                kubernetes_config,
            },
            pod_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    /// Get the Pod API
    pub(crate) fn pods(&self) -> Api<Pod> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    /// Get the PVC API
    pub(crate) fn pvcs(&self) -> Api<PersistentVolumeClaim> {
        Api::namespaced(self.client.clone(), &self.namespace)
    }

    pub(crate) fn service_container_prefix(
        &self,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        // 完全分家:pod/PVC 命名前缀优先读 kubernetes_config(自包含 image_tag_prefix),
        // 回退 multi_image_config(过渡期),再回退 service_type.container_prefix() 默认。
        // 避免命名漂移:k8s 配置改了前缀,pod 与 PVC 必须同步用新前缀。
        if let Some(k8s_cfg) = self.config.kubernetes_config.get_service_config(service_type) {
            return Ok(k8s_cfg.container_prefix().to_string());
        }
        let service_key = service_type.to_string();
        if let Some(config) = self
            .config
            .docker_manager_config
            .multi_image_config
            .services
            .get(&service_key)
        {
            return Ok(config.container_prefix().to_string());
        }
        // 最后兜底:service_type 默认前缀(避免命名查询因配置缺失而硬失败)
        Ok(service_type.container_prefix().to_string())
    }

    pub(crate) fn sanitize_k8s_name_part(input: &str) -> String {
        input
            .to_ascii_lowercase()
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' {
                    ch
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string()
    }

    /// Select image based on service type.
    ///
    /// 优先级:env(RCODER_DOCKER_IMAGE* / RCODER_DOCKER_IMAGE_COMPUTER)> `kubernetes_config`
    /// (完全分家后的主数据源)> `multi_image_config`(docker_config,过渡期安全兜底,
    /// 避免旧 chart 未带 kubernetes_config 时选不到镜像)> 硬编码默认值。
    fn select_image(&self, service_type: &ServiceType) -> String {
        // 1. 优先使用环境变量（允许运行时覆盖;deployment.yaml 注入）
        // 注意：ComputerAgentRunner 必须优先检查 RCODER_DOCKER_IMAGE_COMPUTER
        match service_type {
            ServiceType::ComputerAgentRunner => {
                if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE_COMPUTER")
                    && !env_image.is_empty()
                {
                    info!(
                        "[K8S] Using image from RCODER_DOCKER_IMAGE_COMPUTER env: {}",
                        env_image
                    );
                    return env_image;
                }
                if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE")
                    && !env_image.is_empty()
                {
                    info!(
                        "[K8S] Using image from RCODER_DOCKER_IMAGE env: {}",
                        env_image
                    );
                    return env_image;
                }
            }
            _ => {
                if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE")
                    && !env_image.is_empty()
                {
                    info!(
                        "[K8S] Using image from RCODER_DOCKER_IMAGE env: {}",
                        env_image
                    );
                    return env_image;
                }
            }
        }

        // 2. 从 kubernetes_config(完全分家后的主数据源)按平台选镜像
        if let Some(svc) = self.config.kubernetes_config.get_service_config(service_type) {
            let arch = std::env::consts::ARCH;
            let platform = if arch == "aarch64" || arch == "arm64" {
                "linux/arm64"
            } else {
                "linux/amd64"
            };
            if let Some(image) = svc.get_image_for_platform(platform) {
                info!("[K8S] Using image from kubernetes_config: {}", image);
                return image;
            }
        }

        // 3. 过渡期安全兜底:回退到 docker_config.multi_image_config
        // (旧 chart / 旧 config.yml 未带 kubernetes_config 段时,避免选不到镜像)
        warn!(
            "[K8S] kubernetes_config has no image for {}, falling back to multi_image_config (legacy)",
            service_type
        );
        let multi_config = &self.config.docker_manager_config.multi_image_config;
        if let Some(service_config) = multi_config.get_service_config(service_type) {
            // 优先使用 image 字段
            if let Some(ref image) = service_config.image {
                info!("[K8S] Using image from multi_image_config (fallback): {}", image);
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
                info!("[K8S] Using architecture-specific image (fallback): {}", img);
                return img.to_string();
            }
            // 使用默认镜像
            if let Some(ref img) = service_config.default_image {
                info!("[K8S] Using default image (fallback): {}", img);
                return img.clone();
            }
        }

        // 4. 硬编码兜底(env 与 config 都没给)
        warn!("[K8S] No image config found, using hardcoded fallback");
        match service_type {
            // UserApp 实际走 create_deployment（image_override），不走 create_container/select_image
            // 此处兜底与 WebAgentRunner 共用，仅为 match 穷尽
            ServiceType::WebAgentRunner | ServiceType::UserApp => "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder:latest".to_string(),
            ServiceType::ComputerAgentRunner => {
                "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest".to_string()
            }
        }
    }

    /// Build resource requirements for K8s container from ServiceResourceLimits
    fn build_resource_requirements(limits: &ServiceResourceLimits) -> Option<ResourceRequirements> {
        let mut requests: std::collections::BTreeMap<String, Quantity> =
            std::collections::BTreeMap::new();
        let mut lims: std::collections::BTreeMap<String, Quantity> =
            std::collections::BTreeMap::new();

        if let Some(memory) = limits.memory {
            // memory_limit is in bytes, convert to Mi
            let mem_mb = (memory / (1024.0 * 1024.0)) as i64;
            // Quantity is a string wrapper, construct directly with formatted string
            requests.insert("memory".to_string(), Quantity(format!("{}Mi", mem_mb)));
            lims.insert("memory".to_string(), Quantity(format!("{}Mi", mem_mb)));
        }
        if let Some(cpu) = limits.cpu {
            // cpu_limit is core count, format as decimal string
            requests.insert("cpu".to_string(), Quantity(format!("{}", cpu)));
            lims.insert("cpu".to_string(), Quantity(format!("{}", cpu)));
        }
        // ephemeral-storage：限制 overlay 可写层（/tmp、容器可写层等）。
        // 与 PVC 的 storage_size 是两个独立配额；未显式指定时回退到 storage_size 值。
        let es = limits
            .ephemeral_storage_limit
            .clone()
            .or_else(|| limits.storage_size.clone());
        if let Some(es_qty) = es {
            requests.insert("ephemeral-storage".to_string(), Quantity(es_qty.clone()));
            lims.insert("ephemeral-storage".to_string(), Quantity(es_qty));
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

    // ---- kubernetes_config → kube 对象翻译函数(完全分家:卷/挂载/sidecar 由配置驱动) ----

    /// 翻译 kubernetes_config 卷规格 → kube `Volume`
    ///
    /// - 卷名 "workspace" 被拒(builder 硬编码占用)+告警
    /// - HostPath 被策略禁用 → 跳过+告警
    /// - Pvc/ConfigMap 缺 claim_name/config_map_name → 跳过+告警
    /// - 返回 None 表示该卷被丢弃(调用方 flat_map 跳过)
    fn translate_k8s_volume(spec: &K8sVolumeSpec) -> Option<Volume> {
        // 卷名冲突保护:workspace 由 builder 硬编码管理
        if spec.name == "workspace" {
            warn!(
                "[K8S] config volume name 'workspace' is reserved (builder-managed), skipping"
            );
            return None;
        }
        match spec.volume_type {
            K8sVolumeType::EmptyDir => {
                let mut ed = EmptyDirVolumeSource::default();
                if let Some(sl) = &spec.size_limit {
                    ed.size_limit = Some(Quantity(sl.clone()));
                }
                Some(Volume {
                    name: spec.name.clone(),
                    empty_dir: Some(ed),
                    ..Default::default()
                })
            }
            K8sVolumeType::Pvc => {
                let Some(claim_name) = spec.claim_name.clone() else {
                    warn!(
                        "[K8S] pvc volume '{}' missing claim_name, skipping",
                        spec.name
                    );
                    return None;
                };
                Some(Volume {
                    name: spec.name.clone(),
                    persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                        claim_name,
                        read_only: Some(spec.read_only),
                    }),
                    ..Default::default()
                })
            }
            K8sVolumeType::ConfigMap => {
                let Some(cm_name) = spec.config_map_name.clone() else {
                    warn!(
                        "[K8S] configMap volume '{}' missing config_map_name, skipping",
                        spec.name
                    );
                    return None;
                };
                Some(Volume {
                    name: spec.name.clone(),
                    config_map: Some(ConfigMapVolumeSource {
                        name: cm_name,
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
            K8sVolumeType::HostPath => {
                // 策略禁用:hostPath 绑宿主机路径,动态 agent pod 多节点漂移不安全。
                warn!(
                    "[K8S] hostPath volume '{}' is forbidden by policy, skipping",
                    spec.name
                );
                None
            }
        }
    }

    /// 翻译 kubernetes_config 卷挂载规格 → kube `VolumeMount`
    fn translate_k8s_volume_mount(spec: &K8sVolumeMountSpec) -> VolumeMount {
        VolumeMount {
            name: spec.name.clone(),
            mount_path: spec.mount_path.clone(),
            sub_path: spec.sub_path.clone(),
            read_only: Some(spec.read_only),
            ..Default::default()
        }
    }

    /// 翻译 kubernetes_config sidecar 规格 → kube `Container`(与主 agent 同 Pod)
    fn translate_k8s_sidecar(spec: &K8sSidecarSpec) -> K8sContainer {
        K8sContainer {
            name: spec.name.clone(),
            image: Some(spec.image.clone()),
            // 用户硬性要求:imagePullPolicy 必须 IfNotPresent(动态 pod 频繁创建,节点已缓存)
            image_pull_policy: Some(
                spec.image_pull_policy
                    .clone()
                    .unwrap_or_else(|| "IfNotPresent".to_string()),
            ),
            command: if spec.command.is_empty() {
                None
            } else {
                Some(spec.command.clone())
            },
            volume_mounts: Some(
                spec.volume_mounts
                    .iter()
                    .map(Self::translate_k8s_volume_mount)
                    .collect(),
            ),
            resources: Self::build_resource_requirements(&spec.resources),
            ..Default::default()
        }
    }

    /// 根据运行环境获取容器访问地址
    ///
    /// - K8s 环境：使用 K8s Service FQDN
    /// - Docker 环境：使用容器 IP
    fn get_container_access_address(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        _container_ip: &str,
    ) -> String {
        // K8s 环境：使用 K8s Service FQDN
        let svc_name = self
            .agent_service_name(identifier, service_type)
            .unwrap_or_else(|_| format!("{}-{}", service_type, identifier));
        format!(
            "{}.{}.svc.{}",
            svc_name, self.namespace, self.config.cluster_domain
        )
    }

    /// Build container basic info from runtime container info
    async fn build_container_basic_info(
        &self,
        project_id: &str,
        pod_info: &RuntimeContainerInfo,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        // 使用 K8s Service FQDN 而不是 Pod IP
        let access_address = self.get_container_access_address(
            &pod_info.container_name,
            service_type,
            &pod_info.container_ip,
        );

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
                access_address,
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
            storage_size,
            // UserApp 专用字段（image_override/...）由 create_deployment 处理，
            // agent 的 create_container 路径忽略
            ..
        } = params;

        // 确定容器标识符（复用 ServiceType::container_identifier 单一事实源，
        // 与 docker 模式 / handler 层保持一致）。identifier 借自 pod_id/user_id/project_id 之一。
        // ⚠️ 不要在此重写优先级逻辑，否则会与 handler 层不一致 → ensure/chat 造出不同名 pod+PVC。
        let project_id_val = project_id.clone().unwrap_or_default();
        let user_id_val = user_id.clone().unwrap_or_default();
        let identifier: &str = service_type
            .container_identifier(pod_id.as_deref(), user_id.as_deref(), project_id.as_deref())
            .map_err(|e| ContainerRuntimeError::ConfigurationError(e.to_string()))?;

        // Pod 名称：统一使用 pod_name() helper（含 RFC 1123 下划线清理）
        let pod_name = self.pod_name(identifier, &service_type)?;

        // Ensure workspace PVC exists first (NFS-backed, each project/user gets its own PVC)
        // The PVC is backed by NFS Subdir External Provisioner which automatically
        // creates NFS subdirectory per PVC for isolation and automatic cleanup
        // Note: ensure_workspace_pvc waits for PVC Bound state before returning
        //
        // ComputerAgentRunner 例外：复用共享 rcoder-computer-workspace PVC（subPath=user_id → /home/user），
        // 不为每个 user 新建独立空 PVC——否则沙箱 /home/user 看不到 file-server scaffold 的文件
        // （file-server 写到共享卷 /{user_id}/{cId}，cId=project_id）。{user_id} 目录由
        // create_user_workspace 在 create_container 之前创建，故 subPath 挂载必然命中。
        // WebAgentRunner 同样例外：复用共享 rcoder-workspace PVC（subPath 见下方卷选取），
        // 不为每个项目新建独立空 PVC——否则 CephFS 下每卷隔离，终端 pod 看不到主 pod 写入的
        // 项目文件（/app/project_workspace/{projectId}），ws_terminal fail-closed。
        if !matches!(service_type, ServiceType::ComputerAgentRunner | ServiceType::WebAgentRunner) {
            self.ensure_workspace_pvc(identifier, &service_type, storage_size.as_deref())
                .await?;
        }

        // Check if pod already exists and is running
        if let Some(cached) = self.pod_cache.read().await.get(identifier)
            && cached.status == ContainerRuntimeStatus::Running
        {
            info!("[K8S] Pod {} already exists and is running", pod_name);
            return self
                .get_container_info_by_identifier(identifier, &service_type)
                .await?
                .ok_or_else(|| ContainerRuntimeError::ContainerNotFound(identifier.to_string()));
        }

        let service_type_str = service_type.to_string();
        let image = self.select_image(&service_type);

        // Build labels using standard K8s labels
        let labels = build_standard_labels(identifier, &service_type);

        // Build Pod object using k8s-openapi types
        // Note: Pod existence is already checked via cache above.
        // The API-level check (for race conditions) is intentionally omitted here
        // to avoid extra K8s API call overhead. If create() fails with 409 Conflict,
        // the error will propagate and the caller should handle it.

        // Build resource requirements if limits are provided
        let resources = resource_limits
            .as_ref()
            .and_then(Self::build_resource_requirements);

        // Build workspace volume:
        // - ComputerAgentRunner: 共享 rcoder-computer-workspace PVC + subPath=user_id
        //   → 沙箱 /home/user/{cId} = 主pod /app/computer-project-workspace/{user_id}/{cId}
        // - WebAgentRunner: 共享 rcoder-workspace PVC + subPath（默认 workspace）
        //   → <PVC>/{subPath}/{projectId} = 容器内 /app/project_workspace/{projectId}，
        //   与主 rcoder pod 的 /app/project_workspace 挂载严格对齐（项目文件双向可见）。
        //   旧逻辑建每项目独立 PVC（CephFS 每卷隔离 → 空卷），终端 fail-closed。
        // - 其它（未来 ServiceType）: 每项目独立 PVC 兜底。
        let (workspace_pvc, workspace_sub_path) = match service_type {
            ServiceType::ComputerAgentRunner => {
                let shared = std::env::var("RCODER_COMPUTER_WORKSPACE_PVC_NAME")
                    .unwrap_or_else(|_| format!("{}-rcoder-computer-workspace", self.namespace));
                // subPath 必须等于 file-server/rcoder 建目录用的 user_id（cId 的父级）。
                // 用原始 user_id（非 sanitize），与 file-server createWorkspace(userId) 完全一致。
                (shared, Some(user_id_val.to_string()))
            }
            ServiceType::WebAgentRunner => {
                let shared = std::env::var("RCODER_WORKSPACE_PVC_NAME")
                    .unwrap_or_else(|_| format!("{}-rcoder-workspace", self.namespace));
                // subPath 与主 rcoder pod project_workspace 挂载的 subPath 一致（chart 单一 value
                // RCODER_WORKSPACE_SUBPATH 驱动，默认 "workspace"）。空值兜底为 "workspace"。
                let sub_path = std::env::var("RCODER_WORKSPACE_SUBPATH")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "workspace".to_string());
                (shared, Some(sub_path))
            }
            _ => (self.workspace_pvc_name(identifier, &service_type)?, None),
        };

        // ===== 复用接口 storage_size 设 CephFS 工作区配额 (rcoder 集中, 不依赖 agent pod) =====
        // 背景: storage_size 原本语义是 PVC 大小, 但 Web/Computer 跳过 ensure_workspace_pvc 复用共享 PVC,
        //       致其只落 ephemeral-storage, PVC 子目录写入不限。rcoder 已挂共享 PVC 根 (web
        //       /app/project_workspace, computer /app/computer-project-workspace), 用 xattr::set
        //       (libc setxattr -> CephFS MDS 强制) 对 agent 子目录设 ceph.quota.max_bytes。
        //       子目录规则 (含 web 共享容器 tenant/space/project 三级) 见 agent_workspace_quota_dir。
        //       需 cephx 挂载用户 (csi-cephfs-node, mds=allow rw) 对该目录有 write 权限。
        //       失败只 warn 不阻断 (配额设不上退化为不限, 不崩 rcoder)。
        if let Some(ref ss) = storage_size {
            if let Some(bytes) = parse_quantity_to_bytes(ss) {
                if let Some(quota_dir) = agent_workspace_quota_dir(
                    &service_type,
                    isolation_type.as_deref(),
                    tenant_id.as_deref(),
                    space_id.as_deref(),
                    &project_id_val,
                    &user_id_val,
                ) {
                    let _ = std::fs::create_dir_all(&quota_dir);
                    if let Err(e) = xattr::set(
                        &quota_dir,
                        "ceph.quota.max_bytes",
                        bytes.to_string().as_bytes(),
                    ) {
                        warn!(
                            "set CephFS quota failed on {} (cephx 缺 write / 非 cephfs / 目录不存在?): {}",
                            quota_dir, e
                        );
                    }
                }
            } else {
                warn!("parse storage_size {:?} failed, skip CephFS quota", ss);
            }
        }

        // 取 service 配置(完全分家):K8s 优先读 kubernetes_config;docker_config.multi_image_config
        // 仅作过渡期安全兜底(旧 chart 未带 kubernetes_config 段时,保留 workspace 路径/command/env 行为)。
        // volumes / volume_mounts / sidecars 只来自 kubernetes_config(docker_config 无此概念)。
        let k8s_service = self.config.kubernetes_config.get_service_config(&service_type);
        let docker_service = self
            .config
            .docker_manager_config
            .multi_image_config
            .get_service_config(&service_type);

        // workspace 挂载路径(K8s 模式 computer→/home/user, web→/app/project_workspace)
        let workspace_mount_path = k8s_service
            .map(|sc| sc.workspace_container_path())
            .or_else(|| docker_service.map(|sc| sc.workspace_container_path()))
            .unwrap_or_else(|| match service_type {
                ServiceType::ComputerAgentRunner => "/home/user".to_string(),
                _ => "/app/project_workspace".to_string(),
            });

        // 构建 volumes: 硬编码 workspace PVC(保留) + 翻译 kubernetes_config 额外卷
        let mut volumes_vec: Vec<Volume> = vec![Volume {
            name: "workspace".to_string(),
            persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                claim_name: workspace_pvc.clone(),
                read_only: Some(false),
            }),
            ..Default::default()
        }];
        let extra_volumes: Vec<K8sVolumeSpec> =
            k8s_service.map(|s| s.volumes.clone()).unwrap_or_default();
        for v in extra_volumes.iter().flat_map(Self::translate_k8s_volume) {
            volumes_vec.push(v);
        }

        // 构建 volume_mounts: workspace 挂载 + 翻译 kubernetes_config 额外挂载(挂到 agent 容器)
        let mut volume_mounts_vec: Vec<VolumeMount> = vec![VolumeMount {
            name: "workspace".to_string(),
            mount_path: workspace_mount_path,
            sub_path: workspace_sub_path, // computer→Some(user_id)，web→None
            read_only: Some(false),
            ..Default::default()
        }];
        let extra_mounts: Vec<K8sVolumeMountSpec> = k8s_service
            .map(|s| s.volume_mounts.clone())
            .unwrap_or_default();
        for m in extra_mounts.iter().map(Self::translate_k8s_volume_mount) {
            volume_mounts_vec.push(m);
        }

        let volumes = Some(volumes_vec);
        let volume_mounts = Some(volume_mounts_vec);

        // sidecar 容器(只来自 kubernetes_config):如 log-collector tail 容器内日志到 stdout
        let sidecars: Vec<K8sSidecarSpec> =
            k8s_service.map(|s| s.sidecars.clone()).unwrap_or_default();

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
                termination_grace_period_seconds: Some(15),
                containers: {
                    // 主 agent 容器 + 翻译自 kubernetes_config 的 sidecar(如 log-collector)
                    let mut containers_vec = vec![K8sContainer {
                    name: "agent".to_string(),
                    image: Some(image),
                    // IfNotPresent: 动态 pod 频繁创建（每 chat/computer-chat 一个），
                    // 节点已缓存就直接用，避免每次都去 registry 验 token/manifest。
                    // image 更新由主 Deployment 触发拉取（用户做 rollout restart 时），
                    // 主服务用新 image 启动后，动态 pod 跟着用同样的 image 引用。
                    image_pull_policy: Some("IfNotPresent".to_string()),
                    // 启动命令：
                    //   - WebAgentRunner：从 config.yml 的 web-agent-runner.command 读取
                    //     （与 docker-compose 一致）。配置里的 /app/agent-runner-start.sh wrapper
                    //     会先 nohup 拉起 ttyd(7681)，再 exec agent_runner；agent_runner 的
                    //     ws_terminal 中间层(17681)依赖 ttyd 就绪后才会 bind。若不读配置而裸跑
                    //     agent_runner，ttyd 不启动 -> ws_terminal 等 7681 超时 abort ->
                    //     /computer/terminal 终端 WS 连不上。配置缺失时回退裸 agent_runner
                    //     （保留旧行为，至少 pod 能起；rcoder-master 镜像本身没有 CMD/ENTRYPOINT）。
                    //   - ComputerAgentRunner：刻意用 None 走镜像自带 ENTRYPOINT(start-up.sh)。
                    //     注意 config.yml 里 computer-agent-runner.command 写的是裸 agent_runner，
                    //     那是给 docker 运行时用的；K8s 下若改读它会绕过 start-up.sh，丢失 ttyd/VNC，
                    //     因此这里不复用 config.command。
                    command: match service_type {
                        ServiceType::WebAgentRunner => {
                            // 优先 kubernetes_config.command;过渡期回退 docker_config.command;
                            // 都缺则裸跑 agent_runner(保留旧行为,至少 pod 能起)。
                            let cmd = k8s_service
                                .and_then(|sc| {
                                    if sc.command.is_empty() {
                                        None
                                    } else {
                                        Some(sc.command.clone())
                                    }
                                })
                                .or_else(|| {
                                    docker_service.and_then(|sc| {
                                        if sc.command.is_empty() {
                                            None
                                        } else {
                                            Some(sc.command.clone())
                                        }
                                    })
                                })
                                .unwrap_or_else(|| vec!["/app/bin/agent_runner".to_string()]);
                            Some(cmd)
                        }
                        // ComputerAgentRunner / UserApp 用镜像自带 ENTRYPOINT/CMD
                        // （UserApp 实际走 create_deployment，不经此路径）
                        ServiceType::ComputerAgentRunner | ServiceType::UserApp => None,
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
                            // 部署模式标识: start-up.sh 据此 source extra (K8s 下 /home/user 是 PVC, 跳过 bind mount 权限修复)
                            EnvVar {
                                name: "DEPLOY_MODE".to_string(),
                                value: Some("k8s".to_string()),
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
                        // 透传 service environment
                        // (PROJECT_WORKSPACE_BASE/RUST_LOG/SERVICE_MODE/AGENT_PORT 等,
                        //  让 sub-container 行为与 Docker 模式一致)。跳过已硬编码的同名 env。
                        // 合并顺序:docker_config 兜底 → kubernetes_config 覆盖(K8s 主)。
                        const RESERVED: [&str; 6] = [
                            "PROJECT_ID",
                            "USER_ID",
                            "SERVICE_TYPE",
                            "TENANT_ID",
                            "SPACE_ID",
                            "ISOLATION_TYPE",
                        ];
                        let mut merged_env: std::collections::HashMap<String, String> =
                            std::collections::HashMap::new();
                        if let Some(sc) = docker_service {
                            for (k, v) in &sc.environment {
                                merged_env.insert(k.clone(), v.clone());
                            }
                        }
                        if let Some(sc) = k8s_service {
                            for (k, v) in &sc.environment {
                                merged_env.insert(k.clone(), v.clone());
                            }
                        }
                        for (k, v) in &merged_env {
                            if RESERVED.contains(&k.as_str()) {
                                continue;
                            }
                            env_vars.push(EnvVar {
                                name: k.clone(),
                                value: Some(v.clone()),
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
                    // readiness_probe: initialDelay/period 用 1s, agent_runner /health 一返回 200 即 Ready。
                    // 原 initialDelay=3+period=3 会把首次成功探测拖到 ~3-6s (create 阶段慢)。
                    // 实测 startupProbe + readiness period=3 有 handoff 延迟 (startup 通过后还要等 readiness 的 3s 边界),
                    // 不如直接 readiness period=1: 每 1s 探一次, /health=200 后 ~1s 内 Ready。
                    // 稳态每秒一次 /health GET 开销可忽略; failure_threshold=20 容忍启动期 503, 不被误杀。
                    readiness_probe: Some(Probe {
                        http_get: Some(k8s_openapi::api::core::v1::HTTPGetAction {
                            path: Some("/health".to_string()),
                            port: IntOrString::Int(8086),
                            ..Default::default()
                        }),
                        initial_delay_seconds: Some(1),
                        period_seconds: Some(1),
                        timeout_seconds: Some(3),
                        failure_threshold: Some(20),
                        success_threshold: Some(1),
                        ..Default::default()
                    }),
                    // preStop lifecycle hook: 在 kubelet 发送 SIGTERM 之前执行，
                    // 确保 JuiceFS FUSE 卷上的写入 buffer flush 到磁盘，
                    // 减少 FUSE unmount 卡住的概率
                    lifecycle: Some(k8s_openapi::api::core::v1::Lifecycle {
                        pre_stop: Some(k8s_openapi::api::core::v1::LifecycleHandler {
                            exec: Some(k8s_openapi::api::core::v1::ExecAction {
                                command: Some(vec![
                                    "sh".to_string(),
                                    "-c".to_string(),
                                    "sync && sleep 2".to_string(),
                                ]),
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }];
                    // sidecar(只来自 kubernetes_config.services[].sidecars)。
                    // 无配置时 pod = 仅 agent(干净基线)。log-collector 等采集器在 configmap 声明。
                    containers_vec
                        .extend(sidecars.iter().map(Self::translate_k8s_sidecar));
                    containers_vec
                },
                restart_policy: Some("Never".to_string()),
                service_account_name: Some(self.config.service_account_name.clone()),
                ..Default::default()
            }),
            status: None,
        };

        let pp = PostParams::default();
        match self.pods().create(&pp, &pod).await {
            Ok(_) => {
                info!("[K8S] Pod {} created successfully", pod_name);
            }
            Err(kube::Error::Api(ae)) if ae.code == 409 => {
                // 同名 Pod 已存在。校验其 service_type：历史 identifier bug 可能造出
                // 错类型 pod 撞了本请求的 identifier（如旧 chat 用 user_id 命名的 web pod
                // 撞了 computer 的 user_id identifier，pod 名都是 rcoder-k8s-{user_id}）。
                // 不匹配则删旧重建，避免 computer 请求复用到无 VNC 的 web pod。
                let existing_st: Option<ServiceType> = match self.pods().get(&pod_name).await {
                    Ok(p) => p
                        .metadata
                        .labels
                        .as_ref()
                        .and_then(|l| l.get("rcoder.io/service-type"))
                        .and_then(|v| v.parse::<ServiceType>().ok()),
                    Err(_) => None,
                };
                if existing_st.as_ref() == Some(&service_type) {
                    warn!(
                        "[K8S] Pod {} already exists (409), service_type={:?} matches, reusing",
                        pod_name, service_type
                    );
                } else {
                    warn!(
                        "[K8S] Pod {} exists (409) but service_type mismatch (existing={:?}, requested={:?}); deleting stale pod and recreating",
                        pod_name, existing_st, service_type
                    );
                    // stop_container_by_identifier 删 pod+svc+backend（PVC 保留，新 pod 复用数据）
                    if let Err(e) = self
                        .stop_container_by_identifier(identifier, &service_type)
                        .await
                    {
                        warn!(
                            "[K8S] Failed to stop mismatched pod {}: {} (will retry create anyway)",
                            pod_name, e
                        );
                    }
                    self.pods().create(&pp, &pod).await.map_err(|e| {
                        ContainerRuntimeError::ContainerCreationError(format!(
                            "Failed to recreate pod after service_type mismatch: {}",
                            e
                        ))
                    })?;
                    info!(
                        "[K8S] Pod {} recreated with correct service_type={:?}",
                        pod_name, service_type
                    );
                }
            }
            Err(e) => {
                return Err(ContainerRuntimeError::ContainerCreationError(format!(
                    "Failed to create pod: {}",
                    e
                )));
            }
        }

        // Wait for pod to be ready
        self.wait_for_pod_ready(identifier, &service_type).await?;

        // Create K8s Service for Envoy Gateway routing
        self.create_agent_service(identifier, &service_type).await?;

        // Create Backend CRD for Envoy Gateway discovery (non-fatal if Envoy Gateway is not installed)
        if let Err(e) = self.create_backend_crd(identifier, &service_type).await {
            warn!(
                "[K8S] Failed to create Backend CRD for {} (Envoy Gateway may not be installed): {} (continuing)",
                identifier, e
            );
        }

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
        if let Some(cached) = self.pod_cache.read().await.get(identifier)
            && cached.status == ContainerRuntimeStatus::Running
        {
            // get_container_info 只用于 WebAgentRunner
            let service_type = shared_types::ServiceType::WebAgentRunner;
            return Ok(Some(
                self.build_container_basic_info(identifier, cached, &service_type)
                    .await?,
            ));
        }

        // Query K8s API - 使用标准 K8s 标签查询（与 build_standard_labels 一致）
        let search_queries = vec![
            format!("app.kubernetes.io/instance={}", identifier),
            format!("rcoder.io/identifier={}", identifier),
        ];

        for query in search_queries {
            let lp = ListParams::default().labels(&query);
            if let Ok(pods) = self.pods().list(&lp).await
                && let Some(pod) = pods.items.into_iter().next()
            {
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
                    .map(|ts| {
                        chrono::DateTime::from_timestamp(
                            ts.0.as_second(),
                            ts.0.subsec_nanosecond() as u32,
                        )
                        .unwrap_or_else(Utc::now)
                    })
                    .unwrap_or_else(Utc::now);

                // get_container_info 只用于 WebAgentRunner
                let service_type = shared_types::ServiceType::WebAgentRunner;

                let pod_info = RuntimeContainerInfo {
                    container_id: uid,
                    container_name: name,
                    container_ip: pod_ip,
                    status,
                    created_at,
                    env_vars: None,
                };

                // Update cache if running
                if pod_info.status == ContainerRuntimeStatus::Running {
                    self.pod_cache
                        .write()
                        .await
                        .insert(identifier.to_string(), pod_info.clone());
                }

                return Ok(Some(
                    self.build_container_basic_info(identifier, &pod_info, &service_type)
                        .await?,
                ));
            }
        }

        Ok(None)
    }

    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        let info = self.get_container_info(identifier).await?;
        if info.is_some() {
            // Self-heal：异常创建（如 OrbStack sandbox 超时）可能留下"pod 在、svc/backend 丢"
            // 的不一致状态——pod 重试后起来了，但 create_agent_service / create_backend_crd 那
            // 几步没跑完。后续 Chat 走 svc FQDN `{pod}-svc:50051` 会 transport error → GRPC_ERROR；
            // 外部 Envoy 路由（VNC/终端）也因缺 backend CRD 进不来。
            // 两者均幂等（先 get，存在即返回，缺失才建），此处补建缺失资源，避免人工删 pod 介入。
            // 失败仅 warn（get 是读操作，自愈失败不应阻塞读；Envoy 未装时 backend CRD 也会 warn）。
            if let Err(e) = self.create_agent_service(identifier, service_type).await {
                warn!(
                    "[K8S] self-heal: 补建 agent service 失败 identifier={}, service_type={:?} (non-fatal): {}",
                    identifier, service_type, e
                );
            }
            if let Err(e) = self.create_backend_crd(identifier, service_type).await {
                warn!(
                    "[K8S] self-heal: 补建 backend CRD 失败 identifier={}, service_type={:?} (non-fatal): {}",
                    identifier, service_type, e
                );
            }
        }
        Ok(info)
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
        let pod_name = self.pod_name(identifier, service_type)?;
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

        // 2) Query by labels (使用新的标准标签)
        let selector = format!("app.kubernetes.io/instance={}", identifier);
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

        // 3) 兼容旧标签查询（平滑迁移）
        for old_selector in [
            format!("pod_id={}", identifier),
            format!("user_id={}", identifier),
            format!("project_id={}", identifier),
        ] {
            let pods = self
                .pods()
                .list(&ListParams::default().labels(&old_selector).limit(1))
                .await
                .map_err(|e| {
                    ContainerRuntimeError::K8sError(format!(
                        "Failed to list pods with selector '{}': {}",
                        old_selector, e
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
            .find_container(project_id, &ServiceType::WebAgentRunner)
            .await?
            .is_some();
        let computer_exists = self
            .find_container(project_id, &ServiceType::ComputerAgentRunner)
            .await?
            .is_some();

        if rcoder_exists {
            self.stop_container_by_identifier(project_id, &ServiceType::WebAgentRunner)
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
        let total_start = std::time::Instant::now();
        let pod_name = self.pod_name(identifier, service_type)?;

        info!(
            "[K8S] Stopping pod {} (identifier={}, service_type={})",
            pod_name, identifier, service_type
        );

        // ── Step 0: 删除 Backend CRD + K8s Service（Envoy Gateway 清理）──
        //
        // 先删除 Backend CRD，让 Envoy 停止向该 Pod 路由新流量；
        // 再删除 K8s Service，移除 DNS 条目。两者均在 Pod 终止前完成，
        // 确保流量不再进入即将销毁的 Pod。
        if let Err(e) = self.delete_backend_crd(identifier).await {
            warn!(
                "[K8S] Failed to delete Backend CRD for {}: {} (continuing)",
                identifier, e
            );
        }
        if let Err(e) = self.delete_agent_service(identifier, service_type).await {
            warn!(
                "[K8S] Failed to delete Service for {}: {} (continuing)",
                identifier, e
            );
        }

        // ── Step 1: 发送 Pod 删除请求（graceful，grace period = 15s）──
        //
        // 使用 Foreground propagation 确保 Pod 的子资源先于 Pod 被删除。
        // 注意：pods().delete() 是立即返回的异步 API 调用，
        // Pod 在此时仅被标记为 Terminating，尚未真正终止。
        let step_start = std::time::Instant::now();
        let dp = DeleteParams {
            propagation_policy: Some(kube::api::PropagationPolicy::Foreground),
            grace_period_seconds: Some(15),
            ..Default::default()
        };

        match self.pods().delete(&pod_name, &dp).await {
            Ok(_either) => {
                // _either: Left(Pod) = 立即删除 / Right(name) = 等待终止
                info!(
                    "[K8S] Pod {} delete requested (graceful, 15s), took {:.1}s",
                    pod_name,
                    step_start.elapsed().as_secs_f64()
                );
                self.pod_cache.write().await.remove(identifier);

                // ── Step 2: 等待 Pod 完全终止（404）或超时后 force-delete ──
                let step_start = std::time::Instant::now();
                if let Err(e) = self.wait_for_pod_terminated(&pod_name).await {
                    // Pod 终止失败不阻止 PVC 清理：即使 Pod 仍在运行，
                    // PVC 清理有自己的超时和容错机制
                    warn!(
                        "[K8S] wait_for_pod_terminated failed for {}: {} (continuing to PVC cleanup)",
                        pod_name, e
                    );
                }
                info!(
                    "[K8S] Step 2 (wait pod terminated) took {:.1}s",
                    step_start.elapsed().as_secs_f64()
                );
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                info!("[K8S] Pod {} not found, skip delete", pod_name);
            }
            Err(e) => {
                warn!("[K8S] Failed to request delete pod {}: {}", pod_name, e);
            }
        }

        // ── Step 3: PVC 保留策略 ──
        //
        // 不在 stop_container 时删除 PVC，原因：
        // 1. pod_handler 检测到 Pending pod 会调用 stop + recreate，PVC 需要保留给新 pod 复用
        // 2. 用户显式停止 workspace 时，PVC 应保留以便下次启动时数据不丢失
        // 3. 孤立 PVC 的清理由启动时的 cleanup_all 负责（通过 label selector delete_collection）
        info!(
            "[K8S] Pod {} stopped (PVC preserved for reuse), total time: {:.1}s",
            pod_name,
            total_start.elapsed().as_secs_f64()
        );

        Ok(())
    }

    async fn is_container_running(&self, project_id: &str) -> ContainerRuntimeResult<bool> {
        Ok(self
            .find_container(project_id, &ServiceType::WebAgentRunner)
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

            // 从 Pod 的 labels 中提取环境变量信息
            let mut env_vars = std::collections::HashMap::new();
            if let Some(labels) = &metadata.labels {
                if let Some(project_id) = labels.get("project_id") {
                    env_vars.insert("PROJECT_ID".to_string(), project_id.clone());
                }
                if let Some(user_id) = labels.get("user_id") {
                    env_vars.insert("USER_ID".to_string(), user_id.clone());
                }
            }

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
                    .map(|ts| {
                        chrono::DateTime::from_timestamp(
                            ts.0.as_second(),
                            ts.0.subsec_nanosecond() as u32,
                        )
                        .unwrap_or_else(Utc::now)
                    })
                    .unwrap_or_else(Utc::now),
                env_vars: Some(env_vars),
            };
            result.push(pod_info);
        }

        Ok(result)
    }

    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        let mut removed = Vec::new();

        // 获取缓存快照 (identifier, RuntimeContainerInfo)
        let cache_snapshot: Vec<(String, RuntimeContainerInfo)> = self
            .pod_cache
            .read()
            .await
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let checked_count = cache_snapshot.len() as u32;

        for (identifier, container_info) in cache_snapshot {
            // 使用缓存中的 container_name (完整 pod name) 进行 API 查询
            match self.pods().get(&container_info.container_name).await {
                Err(kube::Error::Api(ae)) if ae.code == 404 => {
                    // Pod 不存在，从缓存中移除，并收集信息用于清理关联资源
                    self.pod_cache.write().await.remove(&identifier);
                    removed.push(RemovedContainerInfo {
                        container_name: container_info.container_name.clone(),
                        container_ip: container_info.container_ip.clone(),
                        identifier: identifier.clone(),
                        service_type: ServiceType::WebAgentRunner, // K8s 模式目前只有 RCoder
                    });
                    info!(
                        "[K8S_SYNC] Removed stale pod from cache: {} (identifier={})",
                        container_info.container_name, identifier
                    );
                }
                Ok(_) => {
                    // Pod 存在，无需处理
                }
                Err(e) => {
                    // 其他错误，只记录日志
                    warn!(
                        "[K8S_SYNC] Failed to check pod {}: {}",
                        container_info.container_name, e
                    );
                }
            }
        }

        Ok((checked_count, removed))
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        let total_start = std::time::Instant::now();
        info!(
            "[K8S_CLEANUP] Starting cleanup_all — sequential Backend CRD → Service → Pod → PVC deletion"
        );

        let lp = ListParams::default().labels(RUNTIME_MANAGED_LABEL);

        // ── Step 0: 批量删除 Backend CRD（停止 Envoy 路由）──
        let backends: Api<DynamicObject> = Api::namespaced_with(
            self.client.clone(),
            &self.namespace,
            &super::k8s_backend_crd::backend_api_resource(),
        );
        match backends
            .delete_collection(&DeleteParams::default(), &lp)
            .await
        {
            Ok(_) => info!("[K8S_CLEANUP] Backend CRD delete_collection requested"),
            Err(e) => {
                tracing::warn!(
                    "[K8S_CLEANUP] Backend CRD delete_collection failed: {} (continuing)",
                    e
                );
            }
        }

        // ── Step 0.5: 批量删除 K8s Service ──
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        match services
            .delete_collection(&DeleteParams::default(), &lp)
            .await
        {
            Ok(_) => info!("[K8S_CLEANUP] Service delete_collection requested"),
            Err(e) => {
                tracing::warn!(
                    "[K8S_CLEANUP] Service delete_collection failed: {} (continuing)",
                    e
                );
            }
        }

        // ── Step 1: 获取所有 managed Pod 名称（用于后续等待终止）──
        let pods_to_wait: Vec<String> = self
            .pods()
            .list(&lp)
            .await
            .map_err(|e| {
                ContainerRuntimeError::ConnectionError(format!(
                    "Failed to list pods for cleanup: {}",
                    e
                ))
            })?
            .items
            .iter()
            .filter_map(|pod| pod.metadata.name.clone())
            .collect();

        info!(
            "[K8S_CLEANUP] Found {} managed pods to clean",
            pods_to_wait.len()
        );

        // ── Step 2: 批量删除 Pod（graceful, Foreground 传播）──
        let dp = DeleteParams {
            propagation_policy: Some(kube::api::PropagationPolicy::Foreground),
            grace_period_seconds: Some(15),
            ..Default::default()
        };

        match self.pods().delete_collection(&dp, &lp).await {
            Ok(_) => info!("[K8S CLEANUP] Pod delete_collection requested"),
            Err(e) => {
                tracing::warn!(
                    "[K8S CLEANUP] Pod delete_collection failed: {} (continuing)",
                    e
                );
            }
        }

        // ── Step 3: 等待所有 Pod 完全终止 ──
        // 关键：必须在删除 PVC 之前完成，确保 FUSE 卷已卸载
        let wait_futures: Vec<_> = pods_to_wait
            .iter()
            .map(|pod_name| self.wait_for_pod_terminated(pod_name))
            .collect();

        let wait_results = futures_util::future::join_all(wait_futures).await;
        for (pod_name, result) in pods_to_wait.iter().zip(wait_results.iter()) {
            if let Err(e) = result {
                tracing::warn!(
                    "[K8S CLEANUP] Pod {} termination wait failed: {}",
                    pod_name,
                    e
                );
            }
        }

        // ── Step 4: PVC 清理策略 ──
        //
        // 不在 cleanup_all 中主动删除 PVC，原因：
        // 1. Pod 删除时 K8s PropagationPolicy::Foreground 会级联清理关联的 PVC
        // 2. 主动删除正在被 pod 使用的 PVC 会导致 PVC 卡在 Terminating 状态（pvc-protection finalizer）
        // 3. 多副本部署时，cleanup_all 会误删其他 rcoder 实例正在使用的 PVC
        // 4. Terminating PVC 会导致后续 create_container 失败（409 重试循环）
        info!(
            "[K8S CLEANUP] PVC cleanup skipped — PVCs are cleaned up via K8s cascading deletion when pods are removed"
        );

        // 清理缓存
        self.pod_cache.write().await.clear();

        info!(
            "[K8S CLEANUP] cleanup_all completed in {:.1}s",
            total_start.elapsed().as_secs_f64()
        );
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

    // ===== Deployment 生命周期（UserApp 专用，转调 k8s_deployment.rs 的 inherent 方法）=====
    async fn create_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        // app_id 占据 project_id 字段位
        let app_id = params.project_id.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "create_deployment requires project_id (app_id)".to_string(),
            )
        })?;
        // Gateway 配置：env 注入优先，未注入则用默认（nuwax-gateway / default，匹配部署现状），
        // 避免部署侧未配 env 时静默跳过 HTTPRoute 创建。
        let gateway_name = std::env::var("RCODER_K8S_GATEWAY_NAME")
            .ok()
            .or_else(|| Some("nuwax-gateway".to_string()));
        let gateway_namespace = std::env::var("RCODER_K8S_GATEWAY_NAMESPACE")
            .ok()
            .or_else(|| Some("default".to_string()));
        self.create_app_resources(
            &app_id,
            &params,
            gateway_name.as_deref(),
            gateway_namespace.as_deref(),
        )
        .await?;
        Ok(ContainerBasicInfo {
            container_id: self.app_deployment_name(&app_id),
            container_name: self.app_deployment_name(&app_id),
            container_ip: String::new(),
            internal_port: 0,
            external_port: 0,
            project_id: app_id.clone(),
            status: "Starting".to_string(),
            created_at: Utc::now(),
            service_url: format!(
                "http://{}.{}.svc.{}",
                self.app_service_name(&app_id),
                self.namespace,
                self.config.cluster_domain
            ),
        })
    }

    async fn patch_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        let app_id = params.project_id.clone().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "patch_deployment requires project_id (app_id)".to_string(),
            )
        })?;
        let gateway_name = std::env::var("RCODER_K8S_GATEWAY_NAME")
            .ok()
            .or_else(|| Some("nuwax-gateway".to_string()));
        let gateway_namespace = std::env::var("RCODER_K8S_GATEWAY_NAMESPACE")
            .ok()
            .or_else(|| Some("default".to_string()));
        // SSA re-apply 全部资源（幂等 create-or-update，收敛到新 desired state）
        self.create_app_resources(
            &app_id,
            &params,
            gateway_name.as_deref(),
            gateway_namespace.as_deref(),
        )
        .await?;
        // 清理 update 后不再需要的端口/配置资源（HTTPRoute/NodePort/ConfigMap/Secret orphan）
        self.cleanup_orphan_port_resources(&app_id, &params).await?;
        info!("[K8S-APP] Deployment patched for app: {app_id}");
        Ok(ContainerBasicInfo {
            container_id: self.app_deployment_name(&app_id),
            container_name: self.app_deployment_name(&app_id),
            container_ip: String::new(),
            internal_port: 0,
            external_port: 0,
            project_id: app_id.clone(),
            status: "Starting".to_string(),
            created_at: Utc::now(),
            service_url: format!(
                "http://{}.{}.svc.{}",
                self.app_service_name(&app_id),
                self.namespace,
                self.config.cluster_domain
            ),
        })
    }

    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        self.scale_app(app_id, replicas).await
    }

    async fn restart_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        self.restart_app(app_id).await
    }

    async fn delete_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        self.delete_app_resources(app_id).await
    }

    async fn get_deployment_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        self.get_app_status(app_id).await
    }

    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        self.list_app_status().await
    }

    async fn get_app_logs(
        &self,
        app_id: &str,
        tail: u32,
        timestamps: bool,
    ) -> ContainerRuntimeResult<Vec<ContainerLogEntry>> {
        self.app_logs(app_id, tail, timestamps).await
    }

    async fn stream_app_logs(
        &self,
        app_id: &str,
        tail: u32,
    ) -> ContainerRuntimeResult<container_runtime_api::mpsc::Receiver<ContainerLogEntry>> {
        self.stream_app_logs_inner(app_id, tail).await
    }

    async fn get_app_events(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::AppEventInfo>> {
        self.app_events(app_id).await
    }

    async fn validate_app_prerequisites(&self) -> ContainerRuntimeResult<()> {
        // RBAC 探测：list deployments（limit 1）。403 = ClusterRole 缺 apps/deployments 权限。
        // 明确报错指向部署侧 RBAC，避免创建 app 时静默 403。
        use k8s_openapi::api::apps::v1::Deployment;
        use kube::api::{Api, ListParams};
        let deploy_api: Api<Deployment> = Api::namespaced(self.client.clone(), &self.namespace);
        match deploy_api.list(&ListParams::default().limit(1)).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 403 => Err(ContainerRuntimeError::ConfigurationError(
                "RBAC 403：rcoder ServiceAccount 缺 apps/deployments 权限，app 管理将无法创建 Deployment。\
                 请在 ClusterRole 补 deployments/httproutes/configmaps/secrets 权限"
                    .to_string(),
            )),
            Err(e) => {
                tracing::warn!(
                    "[K8S-APP] 前置校验 list deployments 失败（非 403，可能 API Server 暂时不可达，跳过）: {}",
                    e
                );
                Ok(())
            }
        }
    }
}

/// 纯函数单元测试:translate_k8s_volume / translate_k8s_volume_mount / translate_k8s_sidecar。
///
/// 这些函数编码了 K8s 动态 pod 的**安全策略**,必须测:
/// - HostPath 禁用(返回 None 跳过)
/// - 卷名 "workspace" 冲突拒绝(builder 硬编码占用)
/// - Pvc/ConfigMap 缺必要字段跳过
/// - sidecar image_pull_policy 默认 IfNotPresent(用户硬性要求)
///
/// 注:select_image / create_container 装配是 &self 方法,依赖 K8s client,此处不覆盖
/// (整个 runtime/ 目录的集成测试是后续议题)。
#[cfg(all(test, feature = "kubernetes"))]
mod tests {
    use super::*;

    // ---- translate_k8s_volume ----

    #[test]
    fn test_translate_volume_emptydir_default() {
        let spec = K8sVolumeSpec {
            name: "container-logs".into(),
            volume_type: K8sVolumeType::EmptyDir,
            ..Default::default()
        };
        let v = KubernetesRuntime::translate_k8s_volume(&spec).expect("emptyDir should translate");
        assert_eq!(v.name, "container-logs");
        let ed = v.empty_dir.expect("empty_dir should be set");
        assert!(ed.size_limit.is_none(), "no size_limit configured");
        // 其它卷源都不该出现
        assert!(v.persistent_volume_claim.is_none());
        assert!(v.config_map.is_none());
    }

    #[test]
    fn test_translate_volume_emptydir_with_size_limit() {
        let spec = K8sVolumeSpec {
            name: "scratch".into(),
            volume_type: K8sVolumeType::EmptyDir,
            size_limit: Some("1Gi".into()),
            ..Default::default()
        };
        let v = KubernetesRuntime::translate_k8s_volume(&spec).expect("emptyDir should translate");
        let ed = v.empty_dir.expect("empty_dir set");
        assert_eq!(ed.size_limit.as_ref().expect("size_limit set").0, "1Gi");
    }

    #[test]
    fn test_translate_volume_pvc_ok() {
        let spec = K8sVolumeSpec {
            name: "data".into(),
            volume_type: K8sVolumeType::Pvc,
            claim_name: Some("my-pvc".into()),
            read_only: true,
            ..Default::default()
        };
        let v = KubernetesRuntime::translate_k8s_volume(&spec).expect("pvc should translate");
        let pvc = v
            .persistent_volume_claim
            .expect("persistent_volume_claim set");
        assert_eq!(pvc.claim_name, "my-pvc");
        assert_eq!(pvc.read_only, Some(true));
        assert!(v.empty_dir.is_none());
    }

    #[test]
    fn test_translate_volume_pvc_missing_claim_name_skipped() {
        let spec = K8sVolumeSpec {
            name: "data".into(),
            volume_type: K8sVolumeType::Pvc,
            // 无 claim_name
            ..Default::default()
        };
        assert!(
            KubernetesRuntime::translate_k8s_volume(&spec).is_none(),
            "pvc without claim_name must be skipped"
        );
    }

    #[test]
    fn test_translate_volume_configmap_ok() {
        let spec = K8sVolumeSpec {
            name: "cfg".into(),
            volume_type: K8sVolumeType::ConfigMap,
            config_map_name: Some("my-cm".into()),
            ..Default::default()
        };
        let v =
            KubernetesRuntime::translate_k8s_volume(&spec).expect("configMap should translate");
        let cm = v.config_map.expect("config_map set");
        assert_eq!(cm.name, "my-cm");
    }

    #[test]
    fn test_translate_volume_configmap_missing_name_skipped() {
        let spec = K8sVolumeSpec {
            name: "cfg".into(),
            volume_type: K8sVolumeType::ConfigMap,
            ..Default::default()
        };
        assert!(
            KubernetesRuntime::translate_k8s_volume(&spec).is_none(),
            "configMap without config_map_name must be skipped"
        );
    }

    #[test]
    fn test_translate_volume_hostpath_forbidden() {
        // HostPath 被策略禁用 → 必须跳过(None),即使配了也要被拒绝
        let spec = K8sVolumeSpec {
            name: "forbidden".into(),
            volume_type: K8sVolumeType::HostPath,
            ..Default::default()
        };
        assert!(
            KubernetesRuntime::translate_k8s_volume(&spec).is_none(),
            "hostPath must be forbidden (policy)"
        );
    }

    #[test]
    fn test_translate_volume_workspace_name_reserved() {
        // "workspace" 卷名被 builder 硬编码占用 → 任何类型的同名卷都该被拒
        for vt in [
            K8sVolumeType::EmptyDir,
            K8sVolumeType::Pvc,
            K8sVolumeType::ConfigMap,
        ] {
            let spec = K8sVolumeSpec {
                name: "workspace".into(),
                volume_type: vt,
                claim_name: Some("x".into()),
                config_map_name: Some("y".into()),
                ..Default::default()
            };
            assert!(
                KubernetesRuntime::translate_k8s_volume(&spec).is_none(),
                "volume name 'workspace' is reserved, must be rejected for {:?}",
                vt
            );
        }
    }

    // ---- translate_k8s_volume_mount ----

    #[test]
    fn test_translate_volume_mount_basic() {
        let spec = K8sVolumeMountSpec {
            name: "container-logs".into(),
            mount_path: "/app/container-logs".into(),
            ..Default::default()
        };
        let m = KubernetesRuntime::translate_k8s_volume_mount(&spec);
        assert_eq!(m.name, "container-logs");
        assert_eq!(m.mount_path, "/app/container-logs");
        assert_eq!(m.sub_path, None);
        assert_eq!(m.read_only, Some(false), "default read_only=false");
    }

    #[test]
    fn test_translate_volume_mount_with_subpath_readonly() {
        let spec = K8sVolumeMountSpec {
            name: "data".into(),
            mount_path: "/data".into(),
            sub_path: Some("user-123".into()),
            read_only: true,
        };
        let m = KubernetesRuntime::translate_k8s_volume_mount(&spec);
        assert_eq!(m.sub_path.as_deref(), Some("user-123"));
        assert_eq!(m.read_only, Some(true));
    }

    // ---- translate_k8s_sidecar ----

    #[test]
    fn test_translate_sidecar_defaults_and_image_pull_policy() {
        let spec = K8sSidecarSpec {
            name: "log-collector".into(),
            image: "registry/alpine:3.22.4".into(),
            command: vec!["/bin/sh".into(), "-c".into(), "sleep 1".into()],
            volume_mounts: vec![K8sVolumeMountSpec {
                name: "container-logs".into(),
                mount_path: "/app/container-logs".into(),
                read_only: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        let c = KubernetesRuntime::translate_k8s_sidecar(&spec);
        assert_eq!(c.name, "log-collector");
        assert_eq!(c.image.as_deref(), Some("registry/alpine:3.22.4"));
        // 用户硬性要求:image_pull_policy 缺省必须 IfNotPresent
        assert_eq!(
            c.image_pull_policy.as_deref(),
            Some("IfNotPresent"),
            "image_pull_policy must default to IfNotPresent"
        );
        // command 非空 → Some
        assert_eq!(c.command.as_deref(), Some(&["/bin/sh".to_string(), "-c".to_string(), "sleep 1".to_string()][..]));
        // volume_mounts 翻译
        let mounts = c.volume_mounts.expect("volume_mounts set");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].name, "container-logs");
        assert_eq!(mounts[0].read_only, Some(true));
        // resources 全 None → build_resource_requirements 返回 None
        assert!(c.resources.is_none());
    }

    #[test]
    fn test_translate_sidecar_empty_command_becomes_none() {
        let spec = K8sSidecarSpec {
            name: "s".into(),
            image: "img".into(),
            ..Default::default()
        };
        let c = KubernetesRuntime::translate_k8s_sidecar(&spec);
        assert!(
            c.command.is_none(),
            "empty command should yield None (use image ENTRYPOINT/CMD)"
        );
    }

    #[test]
    fn test_translate_sidecar_explicit_image_pull_policy_honored() {
        let spec = K8sSidecarSpec {
            name: "s".into(),
            image: "img".into(),
            image_pull_policy: Some("Always".into()),
            ..Default::default()
        };
        let c = KubernetesRuntime::translate_k8s_sidecar(&spec);
        assert_eq!(c.image_pull_policy.as_deref(), Some("Always"));
    }
}
