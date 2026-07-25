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
    ContainerRuntimeResult, ContainerRuntimeStatus, DeploymentStatus, HttpExpose,
    RemovedContainerInfo, RuntimeContainerInfo,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    Container as K8sContainer, ContainerPort, EnvVar, LocalObjectReference, PersistentVolume,
    PersistentVolumeClaim, PersistentVolumeClaimVolumeSource, Pod, PodSecurityContext, PodSpec,
    Probe, ResourceRequirements, Service, Volume, VolumeMount,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
#[cfg(feature = "kubernetes")]
use kube::Config;
#[cfg(feature = "kubernetes")]
use kube::api::{Api, DeleteParams, ListParams, ObjectMeta};
#[cfg(feature = "kubernetes")]
use kube::client::Client;
#[cfg(feature = "kubernetes")]
use shared_types::{
    ContainerBasicInfo, K8sSidecarSpec, K8sVolumeMountSpec, K8sVolumeSpec, ServiceResourceLimits,
    ServiceType,
};
#[cfg(feature = "kubernetes")]
use std::sync::Arc;
#[cfg(feature = "kubernetes")]
use tokio::sync::RwLock;
#[cfg(feature = "kubernetes")]
use tracing::{debug, info, warn};

#[cfg(feature = "kubernetes")]
use super::{
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

/// Kubernetes runtime implementation using kube-rs
#[cfg(feature = "kubernetes")]
pub struct KubernetesRuntime {
    pub(crate) client: Client,
    pub(crate) namespace: String,
    pub(crate) config: KubernetesRuntimeConfig,
    /// Cache for pod information (using RwLock to avoid DashMap deadlocks)
    pub(crate) pod_cache: Arc<RwLock<std::collections::HashMap<String, RuntimeContainerInfo>>>,
    /// identifier -> CephFS subvolumePath 缓存 (resolve_subvolume_path 用)。
    /// subvolumePath 对 PVC 不可变 → 缓存只冷启动填充, 永不失效。
    /// 阶段2: rcoder 挂根聚合访问 agent subvolume (/app/cephfs-root/{subvolumePath}/...)。
    pub(crate) subvolume_path_cache: Arc<RwLock<std::collections::HashMap<String, String>>>,
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
            subvolume_path_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
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

    /// Get the PV API (cluster-scoped)
    ///
    /// 阶段2: 读 PV `csi.volumeAttributes.subvolumePath` (rcoder 挂根聚合)。
    pub(crate) fn pvs(&self) -> Api<PersistentVolume> {
        Api::<PersistentVolume>::all(self.client.clone())
    }

    pub(crate) fn service_container_prefix(
        &self,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        // 完全分家:pod/PVC 命名前缀优先读 kubernetes_config(自包含 image_tag_prefix),
        // 回退 multi_image_config(过渡期),再回退 service_type.container_prefix() 默认。
        // 避免命名漂移:k8s 配置改了前缀,pod 与 PVC 必须同步用新前缀。
        if let Some(k8s_cfg) = self
            .config
            .kubernetes_config
            .get_service_config(service_type)
        {
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
        if let Some(svc) = self
            .config
            .kubernetes_config
            .get_service_config(service_type)
        {
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
                info!(
                    "[K8S] Using image from multi_image_config (fallback): {}",
                    image
                );
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
                info!(
                    "[K8S] Using architecture-specific image (fallback): {}",
                    img
                );
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
    pub(crate) fn build_resource_requirements(
        limits: &ServiceResourceLimits,
    ) -> Option<ResourceRequirements> {
        let mut requests: std::collections::BTreeMap<String, Quantity> =
            std::collections::BTreeMap::new();
        let mut lims: std::collections::BTreeMap<String, Quantity> =
            std::collections::BTreeMap::new();

        // ⚙️ requests/limits 解耦:requests 设超小固定值(仅作 scheduler 调度保障量),
        //    limits 保留配置上限(实际突发由 cgroup 兜底)。
        //    背景:agent-runner 是 AI 代理,常态空闲等响应,资源利用率低;
        //    若 requests=limits(大值),scheduler 严格按 requests 预订 → 节点迅速占满 → Pod Pending。
        //    requests 小 → 支持大量 Pod 超卖调度;limits 大 → 单 Pod 突发不受限(最多 throttle/evict,不崩)。
        //    如需调整,改下方固定值即可(memory/cpu 可压缩性不同,见各字段注释)。
        if let Some(memory) = limits.memory {
            // memory_limit is in bytes, convert to Mi
            let mem_mb = (memory / (1024.0 * 1024.0)) as i64;
            // memory requests 设极小 64Mi:pod 开启了 swap,内存吃紧可换出不易 OOM;
            // requests 仅作 scheduler 调度保障,运行时实际可用到 limits 上限(swap + limits 双兜底)。
            // ⚠️ 代价:节点内存严重紧张时,低 requests + 高实际占用的 pod 可能被优先 evict;
            //    集群内存余量充足(实测 13~34%)且有 swap,风险可控;若频繁被 evict 再调大。
            requests.insert("memory".to_string(), Quantity("64Mi".to_string()));
            lims.insert("memory".to_string(), Quantity(format!("{}Mi", mem_mb)));
        }
        if let Some(cpu) = limits.cpu {
            // cpu_limit is core count, format as decimal string
            // cpu 可压缩:requests 设极小 5m,突发靠 limits(最多 throttle 变慢,不崩)。
            // ⚠️ cpu.shares 按 requests 算,5m 权重极低:节点 CPU 严重争抢时此 pod 会被深度 throttle
            //    (可能慢到 healthcheck 失败/启动超时)。集群 CPU 实测仅 5~7% 闲置,日常可轻松用超 requests;
            //    若遇 pod 启动超时或 healthcheck 失败,先排查是否 CPU 饿死,必要时调大(如 50m)。
            requests.insert("cpu".to_string(), Quantity("5m".to_string()));
            lims.insert("cpu".to_string(), Quantity(format!("{}", cpu)));
        }
        // ephemeral-storage：限制 overlay 可写层（/tmp、容器可写层等）。
        // 与 PVC 的 storage_size 是两个独立配额；未显式指定时回退到 storage_size 值。
        let es = limits
            .ephemeral_storage_limit
            .clone()
            .or_else(|| limits.storage_size.clone());
        if let Some(es_qty) = es {
            // overlay 实际写入很少(业务数据在 PVC),requests 给 512Mi 调度保障,limits 保留配置上限。
            requests.insert(
                "ephemeral-storage".to_string(),
                Quantity("512Mi".to_string()),
            );
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

    /// 获取 K8s 模式 agent 容器的访问地址(Service FQDN)。
    /// Docker 模式不经过此函数(走 docker_runtime,用容器 IP)。
    fn get_container_access_address(&self, identifier: &str) -> String {
        // identifier 是完整 Pod 名(pod_info.container_name = {prefix}-{业务id}),
        // 真实 agent Service 名 = "{pod_name}-svc"(create_agent_service 创建)。
        // 复用 shared_types::build_k8s_service_fqdn 统一 FQDN 格式(与 rcoder 侧 handler、
        // 实际 K8s Service 名对齐)。不要过 agent_service_name/pod_name —— 那会再叠一层
        // service_container_prefix,产生 {prefix}-{prefix}-{id}-svc 双前缀(生产 bug 根因:
        // service_url 多出 rcoder-k8s- 前缀 → permission/cancel/stop transport error)。
        let fqdn = shared_types::build_k8s_service_fqdn(
            identifier,
            &self.namespace,
            &self.config.cluster_domain,
        );
        debug!(
            "[K8S] agent access address: identifier={} -> {}",
            identifier, fqdn
        );
        fqdn
    }

    /// Build container basic info from runtime container info
    async fn build_container_basic_info(
        &self,
        project_id: &str,
        pod_info: &RuntimeContainerInfo,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        // service_url = {sts_name}-svc：container_name 已在 get_container_info 源头剥成 sts_name
        // （agent-runner STS pod 名 {sts_name}-0 的寻址基名），直接拼 Service FQDN。
        let access_address = self.get_container_access_address(&pod_info.container_name);

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

/// 读取 app 暴露相关 env 配置（create/patch 共用，DRY）：
/// gateway_name/gateway_namespace env 注入优先（兜底 nuwax-gateway/default），
/// http_expose 从 RCODER_APP_HTTP_EXPOSE 读取（默认 pingora；无效值 warn 回退，Fail Fast）。
/// 与 app_manager::config 同源，保证 service 层与 K8s 后端一致。
#[cfg(feature = "kubernetes")]
fn read_app_expose_env() -> (Option<String>, Option<String>, HttpExpose) {
    let gateway_name = std::env::var("RCODER_K8S_GATEWAY_NAME")
        .ok()
        .or_else(|| Some("nuwax-gateway".to_string()));
    let gateway_namespace = std::env::var("RCODER_K8S_GATEWAY_NAMESPACE")
        .ok()
        .or_else(|| Some("default".to_string()));
    let http_expose = match std::env::var("RCODER_APP_HTTP_EXPOSE").ok().as_deref() {
        Some("gateway") => HttpExpose::Gateway,
        Some("pingora") | None => HttpExpose::Pingora,
        Some(other) => {
            tracing::warn!(
                "未识别的 RCODER_APP_HTTP_EXPOSE={other:?}，回退 pingora（合法值: pingora|gateway）"
            );
            HttpExpose::Pingora
        }
    };
    (gateway_name, gateway_namespace, http_expose)
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

        // 阶段2 per-agent PVC (CephFS subvolume, ceph-csi 服务端配额, 绕开 client setfattr):
        // 仅隔离容器 (pod_id=None, project/user 级) 且 per_agent_pvc_enabled=true 走 per-agent PVC。
        // 共享容器 (pod_id=Some) 或回滚开关 false → 共享 PVC (选项A 行为)。
        if pod_id.is_none() && shared_types::per_agent_pvc_enabled() {
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

        // workspace PVC:
        // - per-agent (pod_id=None + per_agent_pvc_enabled=true): per-agent PVC (subPath=None)
        // - 共享 (pod_id=Some 或 per_agent_pvc_enabled=false): 共享 PVC + subPath (选项A / 回滚)
        let per_agent = pod_id.is_none() && shared_types::per_agent_pvc_enabled();
        let (workspace_pvc, workspace_sub_path): (String, Option<String>) = if per_agent {
            (self.workspace_pvc_name(identifier, &service_type)?, None)
        } else {
            match &service_type {
                ServiceType::WebAgentRunner => (
                    std::env::var("RCODER_WORKSPACE_PVC_NAME")
                        .unwrap_or_else(|_| format!("{}-rcoder-workspace", self.namespace)),
                    Some(
                        std::env::var("RCODER_WORKSPACE_SUBPATH")
                            .ok()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "workspace".to_string()),
                    ),
                ),
                ServiceType::ComputerAgentRunner => (
                    std::env::var("RCODER_COMPUTER_WORKSPACE_PVC_NAME")
                        .unwrap_or_else(|_| format!("{}-rcoder-computer-workspace", self.namespace)),
                    Some(user_id_val.clone()),
                ),
                _ => (self.workspace_pvc_name(identifier, &service_type)?, None),
            }
        };

        // (阶段2: xattr 目录配额已退役 —— 改用 per-agent subvolume PVC + CSI 服务端配额。
        //  parse_quantity_to_bytes / agent_workspace_quota_dir / xattr crate 已删, 见 Task 2.3)

        // 取 service 配置(完全分家):K8s 优先读 kubernetes_config;docker_config.multi_image_config
        // 仅作过渡期安全兜底(旧 chart 未带 kubernetes_config 段时,保留 workspace 路径/command/env 行为)。
        // volumes / volume_mounts / sidecars 只来自 kubernetes_config(docker_config 无此概念)。
        let k8s_service = self
            .config
            .kubernetes_config
            .get_service_config(&service_type);
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
                        // 注: 此处不配 startup_probe。
                        // 历史: 曾配 startup_probe(initialDelay=5/period=10/failure=12, ~2min 宽限) 防
                        // initdb 慢启动被 liveness 误杀; 但 bebba86 已把 PG initdb 异步化(supervisor 托管),
                        // /health 2s 内 ready, 慢启动根因消除。保留 startup_probe 反成拖累: initialDelay=5 +
                        // period=10 粒度让 Ready 卡在 ~11s(应用早 ready 却要等首探 pod+5s)。去掉后
                        // readiness(period=1) 直接接管, ~3s 内 Ready; 启动期保护由 liveness 兜底
                        // (initialDelay=30 + failure=3×period=10 = 50s 宽限, 远大于 2s ready, 不会被误杀)。
                        // 若日后 agent-runner 首启又变慢(>50s), 再考虑重新引入激进配置的 startup_probe。
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
                    containers_vec.extend(sidecars.iter().map(Self::translate_k8s_sidecar));
                    containers_vec
                },
                // Always(非 Never): agent 容器 OOM/崩溃时由 kubelet 原地重启自愈。
                // Never 下 agent 一死(sidecar 还活着 → pod 仍 Running)rcoder 既不重启也不重建 → 用户中断。
                // rcoder 的 stop/restart/destroy 均走 pods().delete() 整 pod 删, 不依赖 Never;
                // /computer/agent/stop 是 gRPC 取消会话(进程继续), 故 Always 只补崩盘自愈、不冲突。
                restart_policy: Some("Always".to_string()),
                service_account_name: Some(self.config.service_account_name.clone()),
                ..Default::default()
            }),
            status: None,
        };

        // agent-runner 走 StatefulSet（K8s 原生 pod 级自愈）：把上面构造的 pod spec
        // wrap 进 STS（而非裸 Pod）。STS replicas=1 时 pod 被 evict/删除 → 控制器自动重建
        // 同名 pod（挂回同 PVC，数据不丢）；容器级 OOM 仍由 restartPolicy=Always 原地重启。
        // service_type 重名/不匹配由 ensure_agent_statefulset 内部删旧重建处理。
        // UserApp 不走此路径（用 create_deployment）。
        let pod_spec = pod.spec.unwrap_or_default();
        self.ensure_agent_headless_service(identifier, &service_type)
            .await?;
        self.ensure_agent_statefulset(identifier, &service_type, pod_spec, 1)
            .await?;

        // Wait for pod to be ready
        self.wait_for_pod_ready(identifier, &service_type).await?;

        // Create K8s Service for Envoy Gateway routing
        self.create_agent_service(identifier, &service_type).await?;

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
            return Ok(Some(
                self.build_container_basic_info(identifier, cached).await?,
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

                let pod_info = RuntimeContainerInfo {
                    container_id: uid,
                    // agent-runner 走 STS：pod 名 = {sts_name}-0，但 container_name 用作寻址基名
                    // （Service FQDN/grpc_addr/backend_addr 都从它派生 `{name}-svc`），故剥 -0 还原
                    // sts_name，否则所有 gRPC/VNC 地址会指向不存在的 {...}-0-svc。bare-pod 残留无
                    // -0 后缀，strip 安全（identity）。实际 pod 名由 agent_pod_name() 按需取。
                    container_name: Self::sts_name_from_pod_name(&name).to_string(),
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
                    self.build_container_basic_info(identifier, &pod_info)
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
            // Self-heal：异常创建（如 OrbStack sandbox 超时）可能留下"pod 在、svc 丢"
            // 的不一致状态——pod 重试后起来了，但 create_agent_service 那步没跑完。
            // 后续 Chat 走 svc FQDN `{pod}-svc:50051` 会 transport error → GRPC_ERROR。
            // create_agent_service 幂等（先 get，存在即返回，缺失才建），此处补建，避免人工删 pod 介入。
            // 失败仅 warn（get 是读操作，自愈失败不应阻塞读）。
            if let Err(e) = self.create_agent_service(identifier, service_type).await {
                warn!(
                    "[K8S] self-heal: 补建 agent service 失败 identifier={}, service_type={:?} (non-fatal): {}",
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

        // STS 实际 pod 名（等待终止用）；pod_name 即 sts_name。
        let agent_pod = self.agent_pod_name(identifier, service_type)?;

        info!(
            "[K8S] Destroying agent StatefulSet {} (identifier={}, service_type={})",
            pod_name, identifier, service_type
        );

        // Step 0: 删除 ClusterIP Service（先摘流量 / 移除 DNS，再销毁 pod）
        if let Err(e) = self.delete_agent_service(identifier, service_type).await {
            warn!("[K8S] Failed to delete ClusterIP Service for {}: {} (continuing)", identifier, e);
        }

        // Step 1: 删除 StatefulSet（Foreground cascade → pod 随之终止）。回收 = 彻底销毁 STS
        // （非 scale 0；scale 0 会留 STS 永不清理）。PVC 保留（数据复用，下次 ensure 重建挂回）。
        if let Err(e) = self.delete_agent_statefulset(identifier, service_type).await {
            warn!("[K8S] Failed to delete StatefulSet {}: {} (continuing)", pod_name, e);
        }

        // Step 2: 等 pod {sts}-0 完全终止（Foreground cascade 异步；等其 404 再继续，
        // 避免与立即重建的新 pod 抢 RWO PVC）。
        if let Err(e) = self.wait_for_pod_terminated(&agent_pod).await {
            warn!("[K8S] wait_for_pod_terminated for {} failed: {} (continuing)", agent_pod, e);
        }

        // Step 3: 删除 headless Service（与 STS/ClusterIP 一并彻底回收）
        if let Err(e) = self.delete_agent_headless_service(identifier, service_type).await {
            warn!("[K8S] Failed to delete headless Service for {}: {} (continuing)", identifier, e);
        }

        self.pod_cache.write().await.remove(identifier);

        info!(
            "[K8S] agent {} destroyed (STS + ClusterIP/headless svc deleted; PVC preserved for reuse), total time: {:.1}s",
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
                // 同 get 路径：剥 STS ordinal -0，container_name 作寻址基名（见上方注释）。
                container_name: Self::sts_name_from_pod_name(
                    &metadata.name.clone().unwrap_or_default(),
                )
                .to_string(),
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
            // container_name 已是 sts_name（get_container_info 源头剥过 -0）。判"真没了"看 STS
            // 是否存在，不看 pod 404（STS replicas>0 时 pod 被 evict/重建会瞬时空缺，误判清缓存
            // 会中断重建）。
            let sts_name = &container_info.container_name;
            match self.statefulsets().get(sts_name).await {
                Err(kube::Error::Api(ae)) if ae.code == 404 => {
                    // STS 已删 → 真没了；从缓存移除 + 收集（消费方 container_sync 只用 container_ip 清 gRPC 池）
                    self.pod_cache.write().await.remove(&identifier);
                    removed.push(RemovedContainerInfo {
                        container_name: container_info.container_name.clone(),
                        container_ip: container_info.container_ip.clone(),
                        identifier: identifier.clone(),
                        // FIXME: RuntimeContainerInfo 不带 service_type；消费方未用，暂占位。
                        service_type: ServiceType::WebAgentRunner,
                    });
                    info!(
                        "[K8S_SYNC] StatefulSet gone, removed from cache: {} (identifier={})",
                        sts_name, identifier
                    );
                }
                Ok(_) => {
                    // STS 存在（replicas>0 pod 运行/重建中；replicas=0 已停）→ 不动缓存
                }
                Err(e) => {
                    warn!("[K8S_SYNC] Failed to check StatefulSet {}: {}", sts_name, e);
                }
            }
        }

        Ok((checked_count, removed))
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        let total_start = std::time::Instant::now();
        info!("[K8S_CLEANUP] Starting cleanup_all — sequential Service → Pod → PVC deletion");

        let lp = ListParams::default().labels(RUNTIME_MANAGED_LABEL);

        // ── Step 0: 批量删除 K8s Service ──
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

        // 先删 StatefulSet（cascade 删其 pod，且阻止 STS 控制器重建 pod）。
        // agent-runner 现走 STS，若直接删 pod 而 STS 仍在，控制器会立即重建 pod → 永远删不掉。
        match self.statefulsets().delete_collection(&dp, &lp).await {
            Ok(_) => info!("[K8S CLEANUP] StatefulSet delete_collection requested"),
            Err(e) => {
                tracing::warn!(
                    "[K8S CLEANUP] StatefulSet delete_collection failed: {} (continuing)",
                    e
                );
            }
        }

        // 再删 Pod（兜底：清理历史遗留的游离裸 pod，或 STS cascade 未覆盖的残留）
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

        // 清理缓存 (含 subvolume_path_cache — 跨重启 PVC 可能被运维删除重建,
        // 陈旧 cache 导致 resolve 命中旧 subvolPath → rcoder 读老 subvol 而 pod 挂新 PVC → 数据面分裂)
        self.pod_cache.write().await.clear();
        self.subvolume_path_cache.write().await.clear();

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

    async fn resolve_workspace_path(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<String>> {
        // 阶段2: rcoder 静态 PV 挂 CephFS 根 → {RCODER_CEPHFS_ROOT}/{subvolumePath}
        // (subvolumePath 形如 /volumes/csi/<uuid>/<subuuid>, fs 根绝对路径)。
        // file-server 经此聚合路径访问 agent 数据 (tree/git/skills), 不启动 agent pod。
        let cephfs_root =
            std::env::var("RCODER_CEPHFS_ROOT").unwrap_or_else(|_| "/app/cephfs-root".to_string());
        let subvolume_path = self.resolve_subvolume_path(identifier, service_type).await?;
        // subvolumePath 以 / 开头 (fs 绝对路径); trim 防御性处理确保单斜杠拼接
        let sub = subvolume_path.trim_start_matches('/');
        Ok(Some(format!("{cephfs_root}/{sub}")))
    }

    async fn resolve_workspace_path_by_pvcname(
        &self,
        pvc_name: &str,
    ) -> ContainerRuntimeResult<Option<String>> {
        // 阶段3 lazy mv: 与 resolve_workspace_path 同, 但用任意 PVC 名 (共享 PVC 如 rcoder-workspace)
        let cephfs_root =
            std::env::var("RCODER_CEPHFS_ROOT").unwrap_or_else(|_| "/app/cephfs-root".to_string());
        let subvolume_path = self.resolve_subvolume_path_by_pvcname(pvc_name).await?;
        let sub = subvolume_path.trim_start_matches('/');
        Ok(Some(format!("{cephfs_root}/{sub}")))
    }

    /// 枚举某 service_type 的所有 per-app PVC，从 PVC 名反解 identifier（app_id）。
    ///
    /// 用于 storage/query 发现"有持久数据"的 app（含已 delete 但 PVC 保留的孤儿）——
    /// `list_deployments` 只能拿运行中的，PVC 才是持久数据的真源。
    /// PVC 名格式见 `workspace_pvc_name`：`{sanitize(container_prefix)}-{identifier(_→-)}-workspace`，
    /// identifier 经 app_id 校验已是 DNS-1123（无下划线），故反解结果即原 identifier。
    async fn list_workspace_identifiers(
        &self,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Vec<String>> {
        let selector = format!("service_type={}", service_type);
        let list = self
            .pvcs()
            .list(&ListParams::default().labels(&selector))
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!(
                    "list_workspace_identifiers: list PVC failed (service_type={}): {}",
                    service_type, e
                ))
            })?;
        // 前缀/后缀与 workspace_pvc_name 保持一致，反解中间段为 identifier
        let prefix = format!(
            "{}-",
            Self::sanitize_k8s_name_part(&self.service_container_prefix(service_type)?)
        );
        let suffix = "-workspace";
        let mut ids = Vec::with_capacity(list.items.len());
        for pvc in list.items {
            if let Some(name) = pvc.metadata.name.as_deref()
                && let Some(mid) = name
                    .strip_prefix(prefix.as_str())
                    .and_then(|s| s.strip_suffix(suffix))
            {
                ids.push(mid.to_string());
            }
        }
        Ok(ids)
    }

    async fn ensure_workspace(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        storage_size: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        // 复用 K8sPvcOps::ensure_workspace_pvc (幂等: active→复用 / not_found→创建)
        self.ensure_workspace_pvc(identifier, service_type, storage_size)
            .await
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
        let (gateway_name, gateway_namespace, http_expose) = read_app_expose_env();
        self.create_app_resources(
            &app_id,
            &params,
            gateway_name.as_deref(),
            gateway_namespace.as_deref(),
            http_expose,
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
            // UserApp service_url: 传 app_deployment_name(不含 -svc),由 build_k8s_service_fqdn
            // 追加单层 -svc。【不要】传 app_service_name(已含 -svc) → -svc-svc 双后缀。
            service_url: format!(
                "http://{}",
                shared_types::build_k8s_service_fqdn(
                    &self.app_deployment_name(&app_id),
                    &self.namespace,
                    &self.config.cluster_domain,
                ),
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
        let (gateway_name, gateway_namespace, http_expose) = read_app_expose_env();
        // SSA re-apply 全部资源（幂等 create-or-update，收敛到新 desired state）
        self.create_app_resources(
            &app_id,
            &params,
            gateway_name.as_deref(),
            gateway_namespace.as_deref(),
            http_expose,
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
            // UserApp service_url: 传 app_deployment_name(不含 -svc),由 build_k8s_service_fqdn
            // 追加单层 -svc。【不要】传 app_service_name(已含 -svc) → -svc-svc 双后缀。
            service_url: format!(
                "http://{}",
                shared_types::build_k8s_service_fqdn(
                    &self.app_deployment_name(&app_id),
                    &self.namespace,
                    &self.config.cluster_domain,
                ),
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

    async fn get_app_resource_usage(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<container_runtime_api::ResourceUsage> {
        self.app_resource_usage(app_id).await
    }

    async fn exec(
        &self,
        app_id: &str,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        self.app_exec(app_id, command).await
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
