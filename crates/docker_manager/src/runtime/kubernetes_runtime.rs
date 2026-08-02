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
    AgentContainerRuntime, ContainerCreateParams, ContainerLogEntry, ContainerRuntimeError,
    ContainerRuntimeResult, ContainerRuntimeStatus, ContainerSpecSnapshot, DeploymentStatus,
    HttpExpose, RemovedContainerInfo, RuntimeContainerInfo, UserAppDeploymentRuntime,
    WorkspaceRuntime,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    PersistentVolume, PersistentVolumeClaim, Pod, ResourceRequirements,
};
#[cfg(feature = "kubernetes")]
use kube::Config;
#[cfg(feature = "kubernetes")]
use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind, ListParams};
#[cfg(feature = "kubernetes")]
use kube::client::Client;
#[cfg(feature = "kubernetes")]
use shared_types::{ContainerBasicInfo, ServiceResourceLimits, ServiceType};
#[cfg(feature = "kubernetes")]
use std::sync::Arc;
#[cfg(feature = "kubernetes")]
use tokio::sync::RwLock;
#[cfg(feature = "kubernetes")]
use tracing::{debug, info, warn};

#[cfg(feature = "kubernetes")]
use super::k8s_pvc::K8sPvcOps;
#[cfg(feature = "kubernetes")]
use crate::types::DockerManagerConfig;
#[cfg(feature = "kubernetes")]
// 全键：Pod/Service 经 build_standard_labels 写入的是 app.kubernetes.io/managed-by
// （K8s 惯例）。裸 key "managed-by" 只历史性地写在 PVC/Backend CRD 上，
// 会导致 cleanup_all/list_containers 的 label selector 匹配不到 Pod/Service（空跑）。
// 此处与 PVC/Backend CRD 的 label 写入一并对齐到全键。
pub(crate) const RUNTIME_MANAGED_LABEL: &str = "app.kubernetes.io/managed-by=rcoder-runtime";

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

    /// Get the HTTPRoute API（gateway.networking.k8s.io/v1 动态资源；apply / delete by name / label 扫 / 条件删共用）
    pub(crate) fn httproute_api(&self) -> Api<DynamicObject> {
        let gvk = GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute");
        let ar = ApiResource::from_gvk(&gvk);
        Api::namespaced_with(self.client.clone(), &self.namespace, &ar)
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
    pub(crate) fn select_image(&self, service_type: &ServiceType) -> String {
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
            // UserAppBuilder 复用 agent-runner 镜像(含 file-server embed + build 工具链),与
            // ComputerAgentRunner 同源。只读 RCODER_DOCKER_IMAGE_COMPUTER(= agent-runner 镜像),
            // 绝不能读 RCODER_DOCKER_IMAGE(= rcoder 主镜像)——后者默认 CMD 是 node REPL,不是 agent_runner,
            // 会导致 builder pod 落入 node 交互式 shell 而非跑 agent_runner + 内嵌 file-server。
            ServiceType::UserAppBuilder => {
                if let Ok(env_image) = std::env::var("RCODER_DOCKER_IMAGE_COMPUTER")
                    && !env_image.is_empty()
                {
                    info!(
                        "[K8S] UserAppBuilder using agent-runner image from RCODER_DOCKER_IMAGE_COMPUTER env: {}",
                        env_image
                    );
                    return env_image;
                }
                // COMPUTER env 未设 → 落到 step 2 读 kubernetes_config.user-app-builder.image
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
            // UserAppBuilder 复用 dev-rcoder-agent-runner 镜像(与 ComputerAgentRunner 同镜像)
            ServiceType::ComputerAgentRunner | ServiceType::UserAppBuilder => {
                "nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/dev/rcoder-agent-runner:latest".to_string()
            }
        }
    }

    /// Build resource requirements for K8s container from ServiceResourceLimits。
    ///
    /// 委派给共享 `build_decoupled_resources`（与 UserApp 侧 `build_app_resource_requirements`
    /// 共用 requests/limits 解耦策略，值一致）。仅在此做入参转换：ServiceResourceLimits 的
    /// memory(bytes f64）/cpu（核数 f64）归一化为 K8s Quantity 字符串。
    pub(crate) fn build_resource_requirements(
        limits: &ServiceResourceLimits,
    ) -> Option<ResourceRequirements> {
        // memory 字节 → Mi；cpu 核数 → 十进制字符串（K8s Quantity 原生接受）
        let cpu = limits.cpu.map(|c| format!("{}", c));
        let memory = limits
            .memory
            .map(|bytes| format!("{}Mi", (bytes / (1024.0 * 1024.0)) as i64));
        // ephemeral-storage：优先 ephemeral_storage_limit，回退 storage_size（与 PVC storage_size
        // 是两个独立配额；未显式指定时回退，与 app 侧对称）
        let ephemeral = limits
            .ephemeral_storage_limit
            .clone()
            .or_else(|| limits.storage_size.clone());
        super::k8s_app_helpers::build_decoupled_resources(cpu, memory, ephemeral)
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
    pub(crate) async fn build_container_basic_info(
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
impl AgentContainerRuntime for KubernetesRuntime {
    async fn create_container(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.create_agent_container(params).await
    }

    async fn get_container_info(
        &self,
        identifier: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.get_container_info_inner(identifier).await
    }

    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.get_container_info_by_identifier_inner(identifier, service_type)
            .await
    }

    async fn find_container(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        self.find_container_inner(identifier, service_type).await
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
        self.stop_container_by_identifier_inner(identifier, service_type)
            .await
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
        self.list_containers_inner().await
    }

    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        self.sync_states_inner().await
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        self.cleanup_all_inner().await
    }

    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        // Try to list pods as a health check
        let lp = ListParams::default().limit(1);
        self.pods().list(&lp).await.map_err(|e| {
            ContainerRuntimeError::ConnectionError(format!("K8s health check failed: {}", e))
        })?;
        Ok(())
    }

    async fn restart_container_inplace(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        // 委派到 k8s_agent_pod 的 inherent 实现（沿用 get_deployment_status→get_app_status 的
        // 「委派→inherent」模式）。不委派则命中 trait 默认（NotImplemented）→ pod_restart 回落慢路径。
        self.restart_agent_container_inplace(identifier, service_type)
            .await
    }
}

#[cfg(feature = "kubernetes")]
#[async_trait]
impl WorkspaceRuntime for KubernetesRuntime {
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
        let subvolume_path = self
            .resolve_subvolume_path(identifier, service_type)
            .await?;
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

    async fn destroy_app_pvc(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        // 委派 K8sPvcOps::destroy_workspace_pvc (service_type=UserApp; 仅 UserApp 走此路径,
        // agent PVC 永不删)。trait 方法默认 no-op, Docker 不覆盖。
        self.destroy_workspace_pvc(app_id, &ServiceType::UserApp)
            .await
    }
}

#[cfg(feature = "kubernetes")]
#[async_trait]
impl UserAppDeploymentRuntime for KubernetesRuntime {
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

    async fn patch_recycle_policy(
        &self,
        app_id: &str,
        recycle_enabled: Option<bool>,
        idle_timeout_seconds: Option<u64>,
    ) -> ContainerRuntimeResult<()> {
        self.patch_app_recycle_policy(app_id, recycle_enabled, idle_timeout_seconds)
            .await
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

    async fn get_app_container_spec(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<ContainerSpecSnapshot> {
        // 委派到 k8s_deployment 的 inherent 实现（UserApp trait 方法统一走「委派→inherent」模式，
        // 与 get_deployment_status→get_app_status 一致）。不委派则会命中 trait 默认实现（空快照）→
        // update 回退失效 → command/env 仍被清空。
        self.read_app_container_spec(app_id).await
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
