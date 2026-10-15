//! Kubernetes runtime implementation
//!
//! This module provides `KubernetesRuntime` that creates pods in Kubernetes
//! instead of Docker containers, enabling rcoder to work in K8s environments.

#[cfg(feature = "kubernetes")]
use async_trait::async_trait;
#[cfg(feature = "kubernetes")]
#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerRuntimeStatus, HttpExpose, RemovedContainerInfo, RuntimeContainerInfo,
    StorageResizeOutcome, WorkspaceRuntime,
};
#[cfg(feature = "kubernetes")]
use kube::Config;
#[cfg(feature = "kubernetes")]
use kube::api::ListParams;
#[cfg(feature = "kubernetes")]
use kube::client::Client;
#[cfg(feature = "kubernetes")]
use shared_types::{ContainerBasicInfo, ServiceType};
#[cfg(feature = "kubernetes")]
use std::sync::Arc;
#[cfg(feature = "kubernetes")]
use tokio::sync::RwLock;
#[cfg(feature = "kubernetes")]
use tracing::info;

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

/// pod_cache 条目的新鲜度包装：记录写入时刻，TTL 过期则视为 miss 走 K8s API，
/// 修复外部 `kubectl delete pod` / STS 重建窗口期内仍返回旧 Running 的问题。
///
/// 携带 service_type：同 identifier 下 STS 族与生产 UserApp 可并存（builder 与
/// 生产 Deployment 同 app_id），读点校验类型、异族条目视为 miss——对齐 Docker
/// 实现 container_query/lookup.rs 的条目类型校验模式。
#[cfg(feature = "kubernetes")]
#[derive(Clone)]
pub(crate) struct CachedPod {
    pub(crate) info: RuntimeContainerInfo,
    pub(crate) service_type: ServiceType,
    pub(crate) cached_at: std::time::Instant,
}

/// pod_cache TTL：超过则视为 miss。30s 平衡缓存收益与外部删除后的可见性窗口。
#[cfg(feature = "kubernetes")]
pub(crate) const POD_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Kubernetes runtime implementation using kube-rs
#[cfg(feature = "kubernetes")]
pub struct KubernetesRuntime {
    pub(crate) client: Client,
    pub(crate) namespace: String,
    pub(crate) config: KubernetesRuntimeConfig,
    /// Cache for pod information (using RwLock to avoid DashMap deadlocks)
    pub(crate) pod_cache: Arc<RwLock<std::collections::HashMap<String, CachedPod>>>,
    /// CephFS subvolumePath 缓存(key=pvc_name,resolve_subvolume_path_by_pvcname 用)。
    /// subvolumePath 对 PVC 不可变 → 命中即安全;cache miss 时查 K8s(PVC→PV→csi.subvolumePath)懒填充。
    /// 失效时机:PVC destroy(destroy_workspace_pvc 等 remove)+ cleanup_all clear。
    /// 阶段2: rcoder 挂根聚合访问 agent subvolume (/app/cephfs-root/{subvolumePath}/...)。
    pub(crate) subvolume_path_cache: Arc<RwLock<std::collections::HashMap<String, String>>>,
    /// 受管共享 Event publisher（批次 C）：Arc 共享，clone 不重建；非阻塞
    /// 提交，单消费者串行发布；Default = inactive（测试构造用）。
    pub(crate) event_publisher: super::k8s_event_publisher::KubernetesEventPublisher,
    /// K05：事件 publisher 计数（观测面/metrics 接线点；可等待关停用）。
    pub(crate) event_counters: Arc<super::k8s_event_publisher::PublisherCounters>,
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
    /// 集群身份（API endpoint + CA 指纹）：builder 执行域 authority 的稳定
    /// 来源，跨副本一致；部署端点变更会使旧执行域回执拒绝确认（保守）。
    pub execution_authority: String,
}

#[cfg(feature = "kubernetes")]
impl KubernetesRuntime {
    /// Create a new Kubernetes runtime
    pub async fn new(config: DockerManagerConfig) -> ContainerRuntimeResult<Self> {
        // Load kube config from environment or in-cluster config
        let kube_config = Config::infer().await.map_err(|e| {
            ContainerRuntimeError::K8sError(format!("Failed to load kube config: {}", e))
        })?;

        // 集群身份在 kube_config 被 move 进 Client 前取样（API endpoint + CA）。
        let execution_authority = super::k8s_native_domain::cluster_authority(
            &kube_config.cluster_url.to_string(),
            kube_config.root_cert.as_deref(),
        );
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
        // deploy-host 宿主机形态 feature 门控默认（仅 env 未设时生效）：本地集群
        // （OrbStack/k3d/k3s）通常为 local-path/RWO，无 NFS；env 显式设置永远优先
        #[cfg(feature = "deploy-host")]
        let (default_storage_class, default_access_mode) = if shared_types::is_deploy_host() {
            ("local-path", "ReadWriteOnce")
        } else {
            ("rcoder-nfs", "ReadWriteMany")
        };
        #[cfg(not(feature = "deploy-host"))]
        let (default_storage_class, default_access_mode) = ("rcoder-nfs", "ReadWriteMany");
        let storage_class = std::env::var("RCODER_K8S_STORAGE_CLASS")
            .unwrap_or_else(|_| default_storage_class.to_string());
        let access_mode = std::env::var("RCODER_K8S_PVC_ACCESS_MODE")
            .unwrap_or_else(|_| default_access_mode.to_string());

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

        // 批次 C：受管 Event publisher 与 runtime 同生命周期（Arc 共享，
        // clone 不重建）；发布失败绝不影响生命周期路径。K05：计数器句柄由
        // runtime 持有（观测面，metrics 接线点），不再弃置。
        let (event_publisher, event_counters) =
            super::k8s_event_publisher::KubernetesEventPublisher::start(client.clone());

        Ok(Self {
            client,
            event_counters,
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
                execution_authority,
            },
            pod_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
            subvolume_path_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
            event_publisher,
        })
    }
}

/// 读取 app 暴露相关 env 配置（create/patch 共用，DRY）：
/// gateway_name/gateway_namespace env 注入优先（兜底 nuwax-gateway/default），
/// http_expose 从 RCODER_APP_HTTP_EXPOSE 读取（默认 pingora；无效值 warn 回退，Fail Fast）。
/// 与 app_manager::config 同源，保证 service 层与 K8s 后端一致。
#[cfg(feature = "kubernetes")]
pub(super) fn read_app_expose_env() -> (Option<String>, Option<String>, HttpExpose) {
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

// 拆分（file-server 大文件范式）：结构体/配置/`new` 与 `read_app_expose_env`
// 留在 mod.rs（`KubernetesRuntime`/`KubernetesRuntimeConfig` 为 runtime 公开
// re-export 根，外部 `use kubernetes_runtime::X` 路径不变）；`agent_runtime`
// `AgentContainerRuntime` trait impl；`workspace_runtime` `WorkspaceRuntime`
// trait impl。trait 方法不加可见性修饰；块内 super:: 兄弟路径已改写为
// crate::runtime::。

#[cfg(feature = "kubernetes")]
mod agent_runtime;
#[cfg(all(test, feature = "kubernetes"))]
mod builder_log_identity_tests;
#[cfg(all(test, feature = "kubernetes"))]
mod create_lease_tests;
#[cfg(feature = "kubernetes")]
mod workspace_runtime;
