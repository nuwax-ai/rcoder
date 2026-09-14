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

#[cfg(feature = "kubernetes")]
#[async_trait]
impl AgentContainerRuntime for KubernetesRuntime {
    async fn acquire_builder_operation(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Box<dyn shared_types::AppOperationLease>> {
        self.acquire_application_operation(app_id, &ServiceType::UserappBuilder)
            .await
    }
    async fn capture_builder_deletion(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<shared_types::BuilderDeletionSnapshot> {
        self.capture_builder(app_id).await
    }
    async fn delete_builder_snapshot(
        &self,
        snapshot: &shared_types::BuilderDeletionSnapshot,
    ) -> ContainerRuntimeResult<()> {
        self.delete_captured_builder(snapshot).await
    }
    async fn inspect_builder_workspace(
        &self,
        snapshot: &shared_types::BuilderDeletionSnapshot,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::UserAppBuilderWorkspaceEndpoint> {
        self.captured_builder_workspace(snapshot, context).await
    }
    async fn inspect_builder_candidate(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        self.capture_builder_compute_with_binding(context, None, true)
            .await
    }

    async fn capture_builder_adoption(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_container_id: &str,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        let target = self
            .capture_builder_compute_with_binding(context, None, true)
            .await?;
        if target.pod.as_ref().map(|pod| pod.uid.as_str()) != Some(expected_container_id)
            || expected_container_id.is_empty()
        {
            return Err(ContainerRuntimeError::Conflict(
                "Physical builder changed before adoption".into(),
            ));
        }
        Ok(target)
    }
    async fn capture_bound_builder_control(
        &self,
        context: &shared_types::UserAppExecutionContext,
        binding: Option<&shared_types::UserAppResourceBinding>,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        self.capture_builder_compute_with_binding(context, binding, false)
            .await
    }

    async fn capture_builder_control(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        self.capture_builder_compute(context).await
    }

    async fn apply_builder_control(
        &self,
        target: &shared_types::BuilderControlTarget,
        restart: bool,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        let result = self.apply_builder_compute(target, restart).await?;
        if let Some(pod) = &target.pod {
            let mut cache = self.pod_cache.write().await;
            if cache.get(&target.context.app_id).is_some_and(|cached| {
                cached.service_type == ServiceType::UserappBuilder
                    && cached.info.container_id == pod.uid
            }) {
                cache.remove(&target.context.app_id);
            }
        }
        Ok(result)
    }

    async fn create_container(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        params.validate_execution_context()?;
        let lease = if params.service_type == ServiceType::UserappBuilder {
            let identifier = params
                .service_type
                .container_identifier(
                    params.pod_id.as_deref(),
                    params.user_id.as_deref(),
                    params.project_id.as_deref(),
                )
                .map_err(|error| ContainerRuntimeError::ConfigurationError(error.to_string()))?;
            if let Some(context) = &params.execution_context {
                context
                    .validate_identity(identifier, params.user_id.as_deref())
                    .map_err(ContainerRuntimeError::ConfigurationError)?;
            }
            Some(
                self.acquire_application_operation_with_context(
                    identifier,
                    &params.service_type,
                    params.execution_context.as_ref(),
                )
                .await?,
            )
        } else {
            None
        };
        if let Some(lease) = lease {
            let runtime = Self {
                client: self.client.clone(),
                namespace: self.namespace.clone(),
                config: self.config.clone(),
                pod_cache: self.pod_cache.clone(),
                subvolume_path_cache: self.subvolume_path_cache.clone(),
            };
            return tokio::spawn(async move {
                let result = if params.resource_binding.is_some() {
                    runtime.resume_bound_builder(&params).await
                } else {
                    runtime.create_agent_container(params).await
                };
                super::builder_completion::finish(lease, result).await
            })
            .await
            .map_err(|e| {
                ContainerRuntimeError::ContainerCreationError(format!(
                    "builder creation worker: {e}"
                ))
            })?;
        }
        self.create_agent_container(params).await
    }

    async fn get_container_info(
        &self,
        identifier: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        // trait 契约本就是 agent 族管理面（WebAgentRunner 语义，见
        // AgentContainerRuntime 文档注释）；显式类型化后 label 查询带 service-type
        // 维度，不再以 instance 单键捞到生产 UserApp pod。
        self.get_container_info_inner(identifier, &ServiceType::WebAgentRunner)
            .await
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

    async fn is_agent_image_drifted(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        self.is_agent_image_drifted_inner(identifier, service_type)
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

    async fn diagnose_agent_pod(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentPodDiagnostic> {
        self.diagnose_agent_pod_inner(identifier, service_type)
            .await
    }
}

#[cfg(feature = "kubernetes")]
#[async_trait]
impl WorkspaceRuntime for KubernetesRuntime {
    async fn workspace_volume_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        // RBD 卷 rcoder 不可挂载（无路径视角）——PVC 名即存储事实
        use super::k8s_pvc::K8sPvcOps;
        self.workspace_pvc_name(identifier, service_type)
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
        // 委派 K8sPvcOps::destroy_workspace_pvc (service_type=Userapp; 仅 Userapp 走此路径,
        // agent PVC 永不删)。trait 方法默认 no-op, Docker 不覆盖。
        // 显式消歧: WorkspaceRuntime trait 也定义了同名方法(见下)。
        let snapshot = self.capture_deletion(app_id, None).await?;
        if snapshot
            .resources
            .iter()
            .any(|r| r.kind == shared_types::AppResourceKind::Deployment)
        {
            return Err(ContainerRuntimeError::Conflict(
                "application compute must be deleted before storage".into(),
            ));
        }
        // 兜底回收存量第二块 `-data` PVC（单卷化前的旧布局；新部署不存在=幂等 no-op）。
        // 失败不吞：半清理状态（数据卷残留=孤儿计费）比整体失败更难对账。
        self.delete_captured(&snapshot, true).await
    }

    async fn destroy_app_storage_snapshot(
        &self,
        snapshot: &shared_types::AppDeletionSnapshot,
    ) -> ContainerRuntimeResult<()> {
        self.delete_captured(snapshot, true).await
    }

    async fn destroy_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        // 消歧: 显式调 K8sPvcOps 同名方法（per-agent PVC 删除的实际实现）
        K8sPvcOps::destroy_workspace_pvc(self, identifier, service_type).await
    }

    async fn capture_app_storage_resize(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<Option<shared_types::UserAppStorageResizeTarget>> {
        self.capture_storage_resize(context).await.map(Some)
    }

    async fn resize_app_storage_target(
        &self,
        target: &shared_types::UserAppStorageResizeTarget,
        new_size: &str,
    ) -> ContainerRuntimeResult<StorageResizeOutcome> {
        self.resize_storage_target(target, new_size).await
    }

    async fn resize_app_storage(
        &self,
        app_id: &str,
        new_size: &str,
    ) -> ContainerRuntimeResult<StorageResizeOutcome> {
        // 委派 K8sPvcOps::resize_app_pvc（读当前值→比较→patch/事实拒绝）。
        // trait 方法默认 no-op, Docker 不覆盖（bind 目录无容量语义）。
        K8sPvcOps::resize_app_pvc(self, app_id, new_size).await
    }
}

#[cfg(all(test, feature = "kubernetes"))]
mod create_lease_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn runtime(client: Client) -> KubernetesRuntime {
        KubernetesRuntime {
            client,
            namespace: "review-test".into(),
            config: KubernetesRuntimeConfig {
                namespace: "review-test".into(),
                cluster_domain: "cluster.local".into(),
                pod_ttl_seconds: None,
                image_pull_secret: None,
                service_account_name: "test".into(),
                nfs_server: "unused".into(),
                nfs_path: "/unused".into(),
                storage_class: "unused".into(),
                access_mode: "ReadWriteOnce".into(),
                docker_manager_config: Default::default(),
                kubernetes_config: Default::default(),
            },
            pod_cache: Default::default(),
            subvolume_path_cache: Default::default(),
        }
    }

    /// 读一个完整 HTTP 请求（head + body），返回 (请求行起的 head, body)。
    async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 2048];
        let (head, offset, length) = loop {
            let n = stream.read(&mut buffer).await.expect("read");
            assert!(n > 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(offset) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&bytes[..offset]).to_string();
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().expect("length"))
                    })
                    .unwrap_or(0);
                break (head, offset + 4, length);
            }
        };
        while bytes.len() < offset + length {
            let n = stream.read(&mut buffer).await.expect("body");
            assert!(n > 0);
            bytes.extend_from_slice(&buffer[..n]);
        }
        (head, bytes[offset..offset + length].to_vec())
    }

    async fn write_reply(stream: &mut tokio::net::TcpStream, code: u16, body: &serde_json::Value) {
        let body = body.to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("response");
    }

    /// create 中途确定性失败（claim builder storage 被 API server 403 拒——线上
    /// 0.1.264 实测形态）时，operation lease 必须被显式释放：Err 只 drop 会把
    /// ConfigMap 锁留在集群里，该 app 的后续 ensure 全部 409（5 把 builder 锁
    /// 全残留的事故）。断言核心 = 失败后到达的锁 DELETE 请求。
    #[tokio::test]
    async fn create_container_releases_operation_lease_when_create_fails() {
        claim_failure(Some(403), true, false).await;
    }

    #[tokio::test]
    async fn create_container_retains_lease_when_claim_response_is_lost() {
        claim_failure(None, false, false).await;
    }

    #[tokio::test]
    async fn create_container_retains_lease_when_claim_returns_server_error() {
        claim_failure(Some(500), false, false).await;
    }

    #[tokio::test]
    async fn cached_builder_propagates_service_write_failure() {
        claim_failure(Some(403), true, true).await;
        claim_failure(None, false, true).await;
    }

    #[tokio::test]
    async fn storage_claim_retries_only_same_uid_conflicts() {
        storage_claim_conflict(false, 409, 2).await;
    }

    #[tokio::test]
    async fn storage_claim_replacement_is_never_patched() {
        storage_claim_conflict(true, 409, 1).await;
    }

    #[tokio::test]
    async fn storage_claim_forbidden_is_not_retried() {
        storage_claim_conflict(false, 403, 1).await;
    }

    #[tokio::test]
    async fn storage_claim_conflict_budget_is_bounded() {
        storage_claim_conflict(false, 409, 4).await;
    }

    async fn storage_claim_conflict(replaced: bool, code: u16, expected_patches: usize) {
        drop(rustls::crypto::ring::default_provider().install_default());
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server =
                tokio::spawn(async move {
                    let mut patches = Vec::new();
                    loop {
                        let accepted = tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            listener.accept(),
                        )
                        .await;
                        let Ok(Ok((mut stream, _))) = accepted else {
                            break;
                        };
                        let (head, body) = read_request(&mut stream).await;
                        assert!(head.contains("persistentvolumeclaims"), "{head}");
                        if head.starts_with("GET ") {
                            let uid = if replaced && !patches.is_empty() {
                                "replacement"
                            } else {
                                "original"
                            };
                            write_reply(
                                &mut stream,
                                200,
                                &serde_json::json!({
                                    "apiVersion":"v1","kind":"PersistentVolumeClaim",
                                    "metadata":{"name":"rcoder-app-builder-review-workspace",
                                        "uid":uid,"resourceVersion":(patches.len()+1).to_string(),
                                        "labels":{"service_type":"user-app-builder"}}
                                }),
                            )
                            .await;
                        } else {
                            assert!(head.starts_with("PATCH "), "{head}");
                            let patch: serde_json::Value = serde_json::from_slice(&body).unwrap();
                            assert_eq!(patch["metadata"]["uid"], "original");
                            assert_eq!(
                                patch["metadata"]["resourceVersion"],
                                (patches.len() + 1).to_string()
                            );
                            patches.push(patch);
                            if expected_patches == 2 && patches.len() == 2 {
                                write_reply(
                                    &mut stream,
                                    200,
                                    &serde_json::json!({
                                        "apiVersion":"v1","kind":"PersistentVolumeClaim",
                                        "metadata":{"uid":"original","resourceVersion":"3"}
                                    }),
                                )
                                .await;
                            } else {
                                write_reply(&mut stream, code, &serde_json::json!({
                                "apiVersion":"v1","kind":"Status","status":"Failure",
                                "reason":"Failure","code":code,"message":"injected rejection"
                            })).await;
                            }
                        }
                    }
                    patches
                });
            let client =
                Client::try_from(Config::new(format!("http://{address}").parse().unwrap()))
                    .unwrap();
            let context = shared_types::UserAppExecutionContext {
                app_id: "review".into(),
                user_id: "owner".into(),
                lifecycle_id: "lifecycle-one".into(),
                operation_id: "admitted-operation".into(),
                executor_id: "executor-one".into(),
                request_fingerprint: "ab".repeat(32),
            };
            let result = runtime(client)
                .claim_builder_storage_with_context("review", Some(&context))
                .await;
            assert_eq!(result.is_ok(), expected_patches == 2, "{result:?}");
            let patches = server.await.unwrap();
            assert_eq!(patches.len(), expected_patches);
            for patch in &patches {
                assert_eq!(
                    patch["metadata"]["annotations"]["rcoder.io/storage-use-operation"],
                    "admitted-operation"
                );
                assert_eq!(
                    patch["metadata"]["annotations"]["rcoder.io/lifecycle-id"],
                    "lifecycle-one"
                );
                assert_eq!(
                    patch["metadata"]["annotations"]["rcoder.io/owner-id"],
                    "owner"
                );
                assert_eq!(
                    patch["metadata"]["annotations"], patches[0]["metadata"]["annotations"],
                    "retry must keep the original operation identity"
                );
            }
        })
        .await
        .expect("claim regression must finish within its total budget");
    }

    async fn claim_failure(code: Option<u16>, should_release: bool, cached: bool) {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let address = listener.local_addr().expect("address");
            let server = tokio::spawn(async move {
                // 前 4 个请求：acquire POST ConfigMap → ensure 探测 GET PVC →
                // claim 读 GET PVC → claim PATCH PVC；缓存场景继续 GET/PATCH Service。
                for _ in 0..if cached { 6 } else { 4 } {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let (head, body) = read_request(&mut stream).await;
                    if head.starts_with("POST ") && head.contains("/configmaps") {
                        let mut object: serde_json::Value =
                            serde_json::from_slice(&body).expect("acquire body");
                        object["metadata"]["uid"] = "lease-owner".into();
                        object["metadata"]["resourceVersion"] = "42".into();
                        write_reply(&mut stream, 200, &object).await;
                    } else if head.starts_with("GET ") && head.contains("persistentvolumeclaims") {
                        write_reply(
                            &mut stream,
                            200,
                            &serde_json::json!({
                                "apiVersion":"v1","kind":"PersistentVolumeClaim",
                                "metadata":{
                                    "name":"rcoder-app-builder-errclaim-workspace",
                                    "labels":{"service_type":"user-app-builder"},
                                    "uid":"pvc-owned","resourceVersion":"42"}
                            }),
                        )
                        .await;
                    } else if head.starts_with("PATCH ") && head.contains("persistentvolumeclaims")
                    {
                        if cached {
                            write_reply(
                                &mut stream,
                                200,
                                &serde_json::json!({
                                    "apiVersion":"v1", "kind":"PersistentVolumeClaim",
                                    "metadata":{"uid":"pvc-owned", "resourceVersion":"43"}
                                }),
                            )
                            .await;
                            continue;
                        }
                        // Receiving the write and closing without a response models an
                        // unknown server outcome, not a rejected request.
                        let Some(code) = code else {
                            continue;
                        };
                        write_reply(
                            &mut stream,
                            code,
                            &serde_json::json!({
                                "apiVersion":"v1","kind":"Status","status":"Failure",
                                "reason":"Failure","code":code,
                                "message":"persistentvolumeclaims is forbidden: cannot patch"
                            }),
                        )
                        .await;
                    } else if cached && head.starts_with("GET ") && head.contains("/services/") {
                        write_reply(
                            &mut stream,
                            200,
                            &serde_json::json!({
                                "apiVersion":"v1", "kind":"Service",
                                "metadata":{"name":"rcoder-app-builder-errclaim-svc"},
                                "spec":{"ports":[]}
                            }),
                        )
                        .await;
                    } else if cached && head.starts_with("PATCH ") && head.contains("/services/") {
                        if let Some(code) = code {
                            write_reply(&mut stream, code, &serde_json::json!({
                                "apiVersion":"v1", "kind":"Status", "status":"Failure",
                                "reason":"Failure", "code":code, "message":"service patch failed"
                            })).await;
                        }
                    } else {
                        panic!("unexpected request: {head}");
                    }
                }
                // 后续请求：失败路径的锁释放 DELETE（本测试的修复断言核心；
                // 回归时（Err 不释放）此处 accept 超时，saw_release 保持 false）。
                let mut saw_release = false;
                if let Ok(Ok((mut stream, _))) =
                    tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept()).await
                {
                    let (head, body) = read_request(&mut stream).await;
                    assert!(
                        head.starts_with("DELETE ")
                            && head.contains("/configmaps/rcoder-operation-builder-errclaim"),
                        "{head}"
                    );
                    let preconditions: serde_json::Value =
                        serde_json::from_slice(&body).expect("release body");
                    assert_eq!(preconditions["preconditions"]["uid"], "lease-owner");
                    assert_eq!(preconditions["preconditions"]["resourceVersion"], "42");
                    write_reply(
                        &mut stream,
                        200,
                        &serde_json::json!({"apiVersion":"v1","kind":"Status",
                        "status":"Success","code":200}),
                    )
                    .await;
                    saw_release = true;
                }
                assert_eq!(
                    saw_release, should_release,
                    "lease release must depend on confirmed rejection, not just Err"
                );
            });
            drop(rustls::crypto::ring::default_provider().install_default());
            let config = Config::new(format!("http://{address}").parse().expect("uri"));
            let runtime = runtime(Client::try_from(config).expect("client"));
            if cached {
                runtime.pod_cache.write().await.insert(
                    "errclaim".into(),
                    CachedPod {
                        info: RuntimeContainerInfo {
                            container_id: "pod-existing".into(),
                            container_name: "rcoder-app-builder-errclaim-0".into(),
                            container_ip: "10.0.0.1".into(),
                            status: ContainerRuntimeStatus::Running,
                            created_at: chrono::Utc::now(),
                            env_vars: None,
                            service_type: Some(ServiceType::UserappBuilder),
                            project_id: None,
                            user_id: None,
                            pod_id: None,
                            app_id: Some("errclaim".into()),
                        },
                        service_type: ServiceType::UserappBuilder,
                        cached_at: std::time::Instant::now(),
                    },
                );
            }
            let params = ContainerCreateParams::builder()
                .project_id("errclaim")
                .user_id("u-lease")
                .service_type(ServiceType::UserappBuilder)
                .storage_size("10Gi")
                .build();
            let result = runtime.create_container(params).await;
            let error = result.expect_err("create must propagate the injected mutation failure");
            assert!(
                error.to_string().contains(if cached {
                    "patch agent service"
                } else {
                    "claim builder storage"
                }),
                "{error}"
            );
            server.await.expect("adapter assertions");
        })
        .await
        .expect("builder rejection exchange exceeded deadline");
    }

    /// write_app_resources 的阶段进度包装：claim 拒绝/未知结果都必须以
    /// CreationAborted 上抛并携带累计进度——确定性拒绝（403）携带整操作
    /// 安全结束证明，未知类（5xx）不得携带（上层保持围栏）。
    async fn app_creation_claim_outcome(
        patch_code: u16,
    ) -> container_runtime_api::CreationProgress {
        drop(rustls::crypto::ring::default_provider().install_default());
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let pvc_reply = serde_json::json!({
                    "apiVersion":"v1","kind":"PersistentVolumeClaim",
                    "metadata":{"name":"rcoder-app-progclaim-workspace",
                        "uid":"pvc-uid","resourceVersion":"7",
                        "labels":{"service_type":"user-app"}}
                });
                // ① ensure GET（active 复用）② claim 读 GET ③ claim PATCH
                for step in 0..3 {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let (head, _body) = read_request(&mut stream).await;
                    assert!(head.contains("persistentvolumeclaims"), "{head}");
                    if head.starts_with("GET ") {
                        write_reply(&mut stream, 200, &pvc_reply).await;
                    } else if step == 2 {
                        write_reply(
                            &mut stream,
                            patch_code,
                            &serde_json::json!({
                                "apiVersion":"v1","kind":"Status","status":"Failure",
                                "reason":"Injected","code":patch_code,"message":"injected"
                            }),
                        )
                        .await;
                    } else {
                        panic!("unexpected request before claim patch: {head}");
                    }
                }
            });
            let client =
                Client::try_from(Config::new(format!("http://{address}").parse().unwrap()))
                    .unwrap();
            let params = ContainerCreateParams::builder()
                .project_id("progclaim")
                .user_id("u-prog")
                .service_type(ServiceType::Userapp)
                .storage_size("10Gi")
                .build();
            let error = runtime(client)
                .write_app_resources("progclaim", &params, None, None, Default::default(), None)
                .await
                .expect_err("claim outcome must abort creation");
            server.await.expect("scripted exchange completed");
            let ContainerRuntimeError::CreationAborted { progress, .. } = &error else {
                panic!("creation failure must carry progress, got: {error}");
            };
            progress.clone()
        })
        .await
        .expect("progress regression must finish within its budget")
    }

    #[tokio::test]
    async fn app_creation_claim_rejection_proves_safe_finish() {
        let progress = app_creation_claim_outcome(403).await;
        assert_eq!(
            progress.failed_at,
            container_runtime_api::CreationStage::StorageClaim
        );
        assert!(progress.definitive_rejection);
        assert!(progress.safe_finish_ok(), "{progress:?}");
        assert!(
            progress
                .retained_idempotent_resources
                .iter()
                .any(|item| item.contains("workspace pvc ensured")),
            "{progress:?}"
        );
        assert!(
            progress
                .retained_idempotent_resources
                .iter()
                .any(|item| item.contains("storage-claim annotations may persist")),
            "部分认领可能性必须显式记录，不得冒充零变更: {progress:?}"
        );
    }

    #[tokio::test]
    async fn app_creation_unknown_claim_outcome_retains_fence() {
        let progress = app_creation_claim_outcome(503).await;
        assert_eq!(
            progress.failed_at,
            container_runtime_api::CreationStage::StorageClaim
        );
        assert!(!progress.definitive_rejection);
        assert!(!progress.safe_finish_ok(), "{progress:?}");
    }
}
