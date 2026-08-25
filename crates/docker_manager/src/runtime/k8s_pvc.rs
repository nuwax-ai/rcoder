//! Kubernetes PVC (Persistent Volume Claim) 生命周期管理
//!
//! 提供 workspace PVC 的创建、等待绑定、删除和 finalizer 检查等功能。
//! 使用 trait extension 模式为 `KubernetesRuntime` 添加 PVC 操作方法。

/// 默认 PVC 存储大小（当请求未指定 storage_size 时使用）
///
/// 50Gi 配合 cephfs 共享存储，作为「接口入参 resource_limits.storage_size」与
/// 「config.yml resource_limits.storage_size」均未指定时的最终兜底
/// （详见 K8sPvcOps::ensure_workspace_pvc）。
#[cfg(feature = "kubernetes")]
const DEFAULT_PVC_STORAGE_SIZE: &str = "50Gi";

#[cfg(feature = "kubernetes")]
use async_trait::async_trait;
#[cfg(feature = "kubernetes")]
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
#[cfg(feature = "kubernetes")]
use k8s_openapi::api::core::v1::{
    PersistentVolumeClaim, PersistentVolumeClaimSpec, VolumeResourceRequirements,
};
#[cfg(feature = "kubernetes")]
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
#[cfg(feature = "kubernetes")]
use kube::api::{ObjectMeta, PostParams};
#[cfg(feature = "kubernetes")]
use shared_types::ServiceType;
#[cfg(feature = "kubernetes")]
use std::collections::BTreeMap;
#[cfg(feature = "kubernetes")]
use tracing::{debug, info, warn};

#[cfg(feature = "kubernetes")]
use super::kubernetes_runtime::KubernetesRuntime;

/// PVC 生命周期管理操作的 trait extension
///
/// 为 `KubernetesRuntime` 添加 workspace PVC 相关方法：
/// - PVC 命名 (`workspace_pvc_name`)
/// - PVC 创建 (`ensure_workspace_pvc`)
/// - PVC 绑定等待 (`wait_for_pvc_bound`)
/// - subvolume 路径解析 (`resolve_subvolume_path`, 阶段2 rcoder 挂根聚合)
/// - PVC 配额扩容 (`resize_workspace_pvc`, 阶段2 配额调整)
///
/// - PVC 销毁 (`destroy_workspace_pvc`, 仅 UserApp 经 REST `storage/destroy` 显式调用)
///
/// PVC 销毁策略: 默认保留 (clear 清内容留 PVC, 可恢复); 仅 `destroy_workspace_pvc`
/// 显式删 PVC + subvolume (释放配额, 不可逆)。agent PVC 仍永不删。
/// 见 `docs/application-management-service-v2-design.md` §5.4。
#[cfg(feature = "kubernetes")]
#[async_trait]
pub(crate) trait K8sPvcOps {
    /// 生成 workspace PVC 名称
    ///
    /// 格式：`{container_prefix}-{sanitized_id}-workspace`
    /// 其中 `sanitized_id` 将下划线替换为连字符以符合 K8s 命名规范
    fn workspace_pvc_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String>;

    /// 确保 workspace PVC 存在，不存在则创建
    ///
    /// 使用 NFS Subdir External Provisioner 自动在 NFS Server 上创建子目录。
    /// 注意：WaitForFirstConsumer 存储类需要 Pod 调度后才会绑定 PVC，
    /// 因此创建后立即返回，不等待 Bound 状态。
    async fn ensure_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        storage_size: Option<&str>,
    ) -> ContainerRuntimeResult<()>;

    /// UserApp 生产运行容器的 per-app 数据卷名。
    ///
    /// 格式：`{container_prefix}-{sanitized_id}-data`（与 workspace 卷同 prefix
    /// 不同后缀）。承载 PG/dbx 持久数据（挂 `/home/user/{app_id}`），与发布卷
    /// （`-workspace`，挂 `/app`）解耦——重置发布卷不再连数据一起清。
    fn app_data_pvc_name(&self, app_id: &str) -> ContainerRuntimeResult<String>;

    /// 确保 UserApp per-app 数据卷存在，不存在则创建。
    ///
    /// RWO + UserApp 域 RBD 存储类（同 workspace 卷，`userapp_storage_class()`）；
    /// label `service_type=user-app-data`（独立于运行卷的 user-app——RBD PV 无
    /// `csi.volumeAttributes.subvolumePath`，混入运行卷 label 会被 CephFS
    /// subvolumePath 解析逻辑误处理）。
    async fn ensure_app_data_pvc(
        &self,
        app_id: &str,
        storage_size: Option<&str>,
    ) -> ContainerRuntimeResult<()>;

    /// 销毁 UserApp per-app 数据卷（app purge 时随 `-workspace` 卷一并回收）。
    /// 幂等：PVC 不存在返回 Ok。
    async fn destroy_app_data_pvc(&self, app_id: &str) -> ContainerRuntimeResult<()>;

    /// 等待 PVC 进入 Bound 状态
    ///
    /// 保留用于 WaitForFirstConsumer 模式下切换为预绑定策略时使用。
    #[allow(dead_code)]
    async fn wait_for_pvc_bound(&self, pvc_name: &str) -> ContainerRuntimeResult<()>;

    /// 解析 identifier 对应 workspace PVC 的 CephFS subvolume 路径
    ///
    /// 读 PVC (`spec.volumeName`) → PV (`csi.volumeAttributes.subvolumePath`)。
    /// subvolumePath 形如 `/volumes/csi/<uuid>/<subuuid>` (CephFS fs 根绝对路径),
    /// 对 PVC 不可变 → 结果缓存 (`subvolume_path_cache`), 永不失效。
    /// 用于阶段2 rcoder 挂根聚合 (`/app/cephfs-root/{subvolumePath}/...`) 访问 agent 数据。
    async fn resolve_subvolume_path(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String>;

    /// 解析任意 PVC 名的 CephFS subvolume 路径 (阶段3 lazy mv 用)
    ///
    /// 与 `resolve_subvolume_path` 同, 但直接接受 PVC 名 (共享 PVC 如 rcoder-workspace,
    /// 非 `workspace_pvc_name` 生成)。cache key = pvc_name。供 rcoder 经挂根做 lazy mv 时
    /// 定位共享 PVC 的 subvolume 根。
    #[allow(dead_code)]
    async fn resolve_subvolume_path_by_pvcname(
        &self,
        pvc_name: &str,
    ) -> ContainerRuntimeResult<String>;

    /// 扩容 workspace PVC (CephFS subvolume 配额调整)
    ///
    /// patch PVC `spec.resources.requests.storage` → ceph-csi external-resizer 自动
    /// 调 `ceph fs subvolume resize` (SC `allowVolumeExpansion=true`)。只扩不能缩。
    /// 阶段2 配额管理: 初始 `ensure_workspace_pvc` 设 requests.storage (subvolume create --size),
    /// 本方法调调整 (subvolume resize)。
    #[allow(dead_code)]
    async fn resize_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        new_size: &str,
    ) -> ContainerRuntimeResult<()>;

    /// 销毁 workspace PVC + CephFS subvolume (释放配额, 不可逆)。
    ///
    /// 仅 UserApp 经 REST `POST /apps/{id}/storage/destroy` 显式调用 (agent PVC 永不删)。
    /// 调用方须保证 app 已 delete (PVC 无 Pod 引用 → pvc-protection finalizer 正常移除,
    /// 不会卡 Terminating)。幂等: PVC 不存在返回 Ok。
    /// 白名单: pvc_name 由 `workspace_pvc_name` 生成, 只删 `{prefix}-{id}-workspace`,
    /// 碰不到共享 PVC (rcoder-workspace / rcoder-computer-workspace)。
    async fn destroy_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()>;
}

/// UserApp 域（生产运行卷 + builder 开发卷）的 storage class（env
/// `RCODER_USERAPP_STORAGE_CLASS` 显式覆盖，兼容回退旧名
/// `RCODER_USERAPP_BUILDER_STORAGE_CLASS`；缺省 None = PVC 不指定
/// storageClassName → 集群 default StorageClass，如 229/19 的 `ceph-rbd`
/// ——Ceph RBD 块存储）。UserApp 两类卷均 RWO 单容器独占：rcoder 不挂卷
/// （零挂载访问，文件操作经容器内 file-server），编译构建/运行数据走块设备，
/// 不经 CephFS 元数据面。
#[cfg(feature = "kubernetes")]
pub(crate) fn userapp_storage_class() -> Option<String> {
    for key in [
        "RCODER_USERAPP_STORAGE_CLASS",
        "RCODER_USERAPP_BUILDER_STORAGE_CLASS",
    ] {
        if let Some(value) = std::env::var(key)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return Some(value);
        }
    }
    None
}

#[cfg(feature = "kubernetes")]
#[async_trait]
impl K8sPvcOps for KubernetesRuntime {
    fn workspace_pvc_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        let prefix = KubernetesRuntime::sanitize_k8s_name_part(
            &self.service_container_prefix(service_type)?,
        );
        let sanitized = identifier.replace('_', "-");
        Ok(format!("{}-{}-workspace", prefix, sanitized))
    }

    async fn ensure_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        storage_size: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;
        // UserApp 域（生产运行卷 + builder 开发卷）: RWO + RBD（env 可覆盖）;
        // 其余 service_type 用全局 access_mode/storage_class。
        let (access_mode, storage_class_name) = match service_type {
            ServiceType::UserApp | ServiceType::UserAppBuilder => {
                ("ReadWriteOnce".to_string(), userapp_storage_class())
            }
            _ => (
                self.config.access_mode.clone(),
                Some(self.config.storage_class.clone()),
            ),
        };
        self.ensure_pvc_core(
            &pvc_name,
            &service_type.to_string(),
            access_mode,
            storage_class_name,
            storage_size.unwrap_or(DEFAULT_PVC_STORAGE_SIZE),
        )
        .await
    }

    fn app_data_pvc_name(&self, app_id: &str) -> ContainerRuntimeResult<String> {
        let prefix = KubernetesRuntime::sanitize_k8s_name_part(
            &self.service_container_prefix(&ServiceType::UserApp)?,
        );
        let sanitized = app_id.replace('_', "-");
        Ok(format!("{}-{}-data", prefix, sanitized))
    }

    async fn ensure_app_data_pvc(
        &self,
        app_id: &str,
        storage_size: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        let pvc_name = self.app_data_pvc_name(app_id)?;
        self.ensure_pvc_core(
            &pvc_name,
            // 独立 label：RBD PV 无 subvolumePath，勿混入 user-app 的 CephFS 解析面
            "user-app-data",
            "ReadWriteOnce".to_string(),
            userapp_storage_class(),
            storage_size.unwrap_or(Self::DEFAULT_APP_DATA_STORAGE_SIZE),
        )
        .await
    }

    async fn wait_for_pvc_bound(&self, pvc_name: &str) -> ContainerRuntimeResult<()> {
        let wait_timeout = std::time::Duration::from_secs(60);
        let start = std::time::Instant::now();
        while start.elapsed() < wait_timeout {
            match self.pvcs().get(pvc_name).await {
                Ok(pvc) => {
                    if pvc.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Bound") {
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

    async fn resolve_subvolume_path(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;
        self.resolve_subvolume_path_by_pvcname(&pvc_name).await
    }

    async fn resolve_subvolume_path_by_pvcname(
        &self,
        pvc_name: &str,
    ) -> ContainerRuntimeResult<String> {
        // cache hit (subvolumePath 对 PVC 不可变,命中即安全;失效靠 PVC destroy 时的 remove。
        // key=pvc_name 覆盖共享+per-agent)
        if let Some(cached) = self
            .subvolume_path_cache
            .read()
            .await
            .get(pvc_name)
            .cloned()
        {
            return Ok(cached);
        }
        let pvc = self.pvcs().get(pvc_name).await.map_err(|e| {
            ContainerRuntimeError::K8sError(format!("Failed to get PVC '{}': {}", pvc_name, e))
        })?;
        let pv_name = pvc
            .spec
            .as_ref()
            .and_then(|s| s.volume_name.clone())
            .filter(|n| !n.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(format!(
                    "PVC '{}' not Bound yet (volumeName empty), cannot resolve subvolumePath",
                    pvc_name
                ))
            })?;
        let pv = self.pvs().get(&pv_name).await.map_err(|e| {
            ContainerRuntimeError::K8sError(format!("Failed to get PV '{}': {}", pv_name, e))
        })?;
        // csi.volumeAttributes["subvolumePath"] (字段名待部署实测确认;
        // 兜底 rootPath, 部分 ceph-csi 版本用此 key)
        let vol_attrs = pv
            .spec
            .as_ref()
            .and_then(|s| s.csi.as_ref())
            .and_then(|csi| csi.volume_attributes.as_ref());
        let subvolume_path = vol_attrs
            .and_then(|a| a.get("subvolumePath").cloned())
            .or_else(|| vol_attrs.and_then(|a| a.get("rootPath").cloned()))
            .ok_or_else(|| {
                ContainerRuntimeError::K8sError(format!(
                    "PV '{}' csi.volumeAttributes has no subvolumePath/rootPath",
                    pv_name
                ))
            })?;
        self.subvolume_path_cache
            .write()
            .await
            .insert(pvc_name.to_string(), subvolume_path.clone());
        debug!(
            "[K8S] resolved subvolumePath for PVC {}: {} (PV {})",
            pvc_name, subvolume_path, pv_name
        );
        Ok(subvolume_path)
    }

    async fn resize_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        new_size: &str,
    ) -> ContainerRuntimeResult<()> {
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;
        // merge patch spec.resources.requests.storage → ceph-csi external-resizer
        // 自动调 `ceph fs subvolume resize` (SC allowVolumeExpansion=true)
        let patch = serde_json::json!({
            "spec": { "resources": { "requests": { "storage": new_size } } }
        });
        let pp = kube::api::PatchParams::default();
        self.pvcs()
            .patch(&pvc_name, &pp, &kube::api::Patch::Merge(&patch))
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!(
                    "Failed to patch PVC '{}' requests.storage to {}: {}",
                    pvc_name, new_size, e
                ))
            })?;
        info!(
            "[K8S] PVC {} resize requested -> {} (ceph-csi auto subvolume resize)",
            pvc_name, new_size
        );
        Ok(())
    }

    async fn destroy_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        // UserAppBuilder 现为 per-app RWO PVC（app 删除 purge 时随容器一并回收,
        // 调用方为 UserApp 域 REST 流程, 符合"agent PVC 永不删"约束的例外面）。
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;
        self.destroy_pvc_core(&pvc_name).await
    }

    async fn destroy_app_data_pvc(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let pvc_name = self.app_data_pvc_name(app_id)?;
        self.destroy_pvc_core(&pvc_name).await
    }
}

impl KubernetesRuntime {
    /// data 卷默认容量（PG + dbx 持久数据，独立于发布卷的 50Gi 默认）。
    const DEFAULT_APP_DATA_STORAGE_SIZE: &'static str = "20Gi";

    /// PVC ensure 核心（workspace 卷与 UserApp data 卷共用）：存在检查/terminating
    /// 等待/SC 漂移可见/创建重试。`service_type_label` 仅作为 PVC label 值。
    async fn ensure_pvc_core(
        &self,
        pvc_name: &str,
        service_type_label: &str,
        access_mode: String,
        storage_class_name: Option<String>,
        storage_size: &str,
    ) -> ContainerRuntimeResult<()> {
        // Check if PVC already exists and its state
        let (pvc_status, existing_sc) = match self.pvcs().get(pvc_name).await {
            Ok(pvc) => {
                let sc = pvc
                    .spec
                    .as_ref()
                    .and_then(|sp| sp.storage_class_name.clone());
                if pvc.metadata.deletion_timestamp.is_some() {
                    // PVC is in Terminating state — it's being deleted.
                    // We must wait for it to be fully removed before creating a new one,
                    // otherwise the new pod will reference a PVC that's about to disappear.
                    info!(
                        "[K8S] PVC {} is in Terminating state, waiting for deletion to complete...",
                        pvc_name
                    );
                    ("terminating", sc)
                } else {
                    info!("[K8S] PVC {} already exists and is active", pvc_name);
                    ("active", sc)
                }
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => ("not_found", None),
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Failed to check PVC '{}': {}",
                    pvc_name, e
                )));
            }
        };

        match pvc_status {
            "active" => {
                // 漂移可见：既有 PVC 的 storageClassName 与期望不一致
                // （env 切换前创建/运维预建）时 warn——静默复用会让存储语义与配置脱节。
                // PVC 未显式设 SC（None）= 用集群 default，不比对。
                let expected_sc = storage_class_name.clone();
                if let (Some(existing), Some(expected)) = (&existing_sc, &expected_sc)
                    && existing != expected
                {
                    warn!(
                        "[K8S] PVC {} storageClassName={existing:?} differs from expected {expected:?} (service_type={service_type_label}):                          reusing existing, storage semantics may drift",
                        pvc_name
                    );
                }
                // PVC exists and is not being deleted — reuse it.
                // WaitForFirstConsumer PVCs will be Bound once a Pod referencing them is scheduled.
                // 漂移可见：既有 PVC 的 accessModes/storageClassName 与本 service_type
                // 期望不一致（env 切换前创建/运维预建）时 warn——静默复用会让 RWO
                // 单容器独占的调度假设与实际存储能力脱节。
                info!(
                    "[K8S] PVC {} already exists, skipping Bound check (WaitForFirstConsumer)",
                    pvc_name
                );
                return Ok(());
            }
            "terminating" => {
                // PVC is being deleted — wait for it to be fully removed, then create a new one.
                let wait_start = std::time::Instant::now();
                let max_wait = std::time::Duration::from_secs(60);
                loop {
                    match self.pvcs().get(pvc_name).await {
                        Err(kube::Error::Api(ae)) if ae.code == 404 => {
                            info!(
                                "[K8S] PVC {} fully deleted after {:.1}s, will create new one",
                                pvc_name,
                                wait_start.elapsed().as_secs_f64()
                            );
                            break;
                        }
                        Ok(_) => {
                            if wait_start.elapsed() > max_wait {
                                // Force delete the PVC if it's stuck
                                warn!(
                                    "[K8S] PVC {} stuck in Terminating for {:.1}s, force deleting",
                                    pvc_name,
                                    wait_start.elapsed().as_secs_f64()
                                );
                                let dp = kube::api::DeleteParams {
                                    grace_period_seconds: Some(0),
                                    ..Default::default()
                                };
                                if let Err(e) = self.pvcs().delete(pvc_name, &dp).await {
                                    warn!("[K8S] force delete PVC {} failed: {}", pvc_name, e);
                                }
                                // Wait a bit more for the API to reflect the deletion
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        Err(e) => {
                            // 非 404 错误（transport/auth/RBAC 瞬时故障）：不假定 PVC 已删，
                            // 继续轮询（对齐 destroy_workspace_pvc），避免误判已删后重建撞 409。
                            // 同样受 max_wait 约束，超时则跳出由上层重建兜底。
                            if wait_start.elapsed() > max_wait {
                                warn!(
                                    "[K8S] PVC {} get kept failing (non-404) for {:.1}s; stop waiting",
                                    pvc_name,
                                    wait_start.elapsed().as_secs_f64()
                                );
                                break;
                            }
                            warn!(
                                "[K8S] PVC {} get failed during Terminating wait, keep polling: {}",
                                pvc_name, e
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                    }
                }
                // PVC 已删 (404/强删/error), 将重建新 PVC (新 CephFS subvolume UUID) →
                // invalidate subvolPath 缓存 (旧 subvolPath 不再对应此 PVC; 不清则 resolve 命中脏值,
                // rcoder 经挂根读老 subvol 而 pod 挂新 PVC → 数据面分裂)
                self.subvolume_path_cache.write().await.remove(pvc_name);
            }
            _ => {
                // "not_found" — PVC 不存在, 将创建。清可能陈旧的 subvolPath cache
                // (运维 kubectl delete pvc 后重建场景: 新 PVC 新 ceph subvol UUID,
                // 旧 cache 导致 resolve 返回旧 subvol → 数据面分裂)
                self.subvolume_path_cache.write().await.remove(pvc_name);
            }
        }
        // If not found or terminated, create it (falls through to creation logic below)

        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(pvc_name.to_string()),
                namespace: Some(self.namespace.clone()),
                labels: Some({
                    let mut m = BTreeMap::new();
                    m.insert("app".to_string(), "rcoder".to_string());
                    m.insert(
                        "app.kubernetes.io/managed-by".to_string(),
                        "rcoder-runtime".to_string(),
                    );
                    m.insert("service_type".to_string(), service_type_label.to_string());
                    m
                }),
                ..Default::default()
            },
            spec: Some(PersistentVolumeClaimSpec {
                // access_mode/storage_class_name 由调用方按卷型决定:
                // UserApp 域（运行卷/开发卷/data 卷）RWO RBD 单容器独占,
                // 其余用全局配置。
                access_modes: Some(vec![access_mode]),
                storage_class_name,
                resources: Some(VolumeResourceRequirements {
                    requests: Some({
                        let mut r = BTreeMap::new();
                        r.insert("storage".to_string(), Quantity(storage_size.to_string()));
                        r
                    }),
                    ..Default::default()
                }),
                volume_name: None,
                ..Default::default()
            }),
            status: None,
        };

        // Retry create up to 5 times to handle race condition:
        // After force-delete, the PVC may still exist in "Deleting" state briefly.
        // K8s returns 409 AlreadyExists in this case.
        let create_start = std::time::Instant::now();
        let max_create_wait = std::time::Duration::from_secs(30);
        loop {
            match self.pvcs().create(&PostParams::default(), &pvc).await {
                Ok(pvc_created) => {
                    info!(
                        "[K8S] PVC {} created successfully",
                        pvc_created.metadata.name.as_deref().unwrap_or("unknown")
                    );
                    return Ok(());
                }
                Err(kube::Error::Api(ae)) if ae.code == 409 => {
                    if create_start.elapsed() > max_create_wait {
                        return Err(ContainerRuntimeError::ContainerCreationError(format!(
                            "Failed to create PVC '{}': still exists after {:.1}s of retries",
                            pvc_name,
                            create_start.elapsed().as_secs_f64()
                        )));
                    }
                    warn!(
                        "[K8S] PVC {} still being deleted (409), retrying in 2s... (elapsed {:.1}s)",
                        pvc_name,
                        create_start.elapsed().as_secs_f64()
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Err(e) => {
                    return Err(ContainerRuntimeError::ContainerCreationError(format!(
                        "Failed to create PVC '{}': {}",
                        pvc_name, e
                    )));
                }
            }
        }
    }

    /// PVC destroy 核心（workspace 卷与 UserApp data 卷共用）：幂等删除 +
    /// 60s Terminating 等待 → 强删 grace=0。
    async fn destroy_pvc_core(&self, pvc_name: &str) -> ContainerRuntimeResult<()> {
        // 幂等: PVC 不存在直接返回成功 (Java 重试 / 对账安全)
        match self.pvcs().get(pvc_name).await {
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                info!(
                    "[K8S] PVC {} not found, destroy is no-op (idempotent)",
                    pvc_name
                );
                self.subvolume_path_cache.write().await.remove(pvc_name);
                return Ok(());
            }
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Failed to get PVC '{}' before destroy: {}",
                    pvc_name, e
                )));
            }
            Ok(_) => {}
        }

        // 发删除请求 (默认 grace period; 调用方保证 app 已 delete → 无 Pod 引用 →
        // pvc-protection finalizer 正常移除)
        match self
            .pvcs()
            .delete(pvc_name, &kube::api::DeleteParams::default())
            .await
        {
            Ok(_) => info!("[K8S] PVC {} delete requested", pvc_name),
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                // 并发已删, 幂等
                self.subvolume_path_cache.write().await.remove(pvc_name);
                return Ok(());
            }
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Failed to delete PVC '{}': {}",
                    pvc_name, e
                )));
            }
        }

        // 等 PVC 完全消失 (复用 ensure_workspace_pvc 的轮询模式: 60s 超时 → 强删 grace=0)。
        // app 已 delete 前提下, pvc-protection finalizer 会正常移除, 通常秒级完成。
        // ⚠️ 与 ensure_workspace_pvc 的关键差异: 非 404 错误 (transport/auth/RBAC 瞬时故障)
        //    不假定"已删"——destroy 不可逆, 误报成功会让用户基于"PVC 已销毁"做错决策
        //    (如缩减 cephfs 容量规划、对账孤儿); 继续轮询直到 404 或超时强删。
        let wait_start = std::time::Instant::now();
        let max_wait = std::time::Duration::from_secs(60);
        loop {
            // 超时 → 强删 + 确认 (Ok/Err 任一累计到超时都走此分支, 抽顶部避免重复)
            if wait_start.elapsed() > max_wait {
                warn!(
                    "[K8S] PVC {} stuck in Terminating for {:.1}s, force deleting",
                    pvc_name,
                    wait_start.elapsed().as_secs_f64()
                );
                let dp = kube::api::DeleteParams {
                    grace_period_seconds: Some(0),
                    ..Default::default()
                };
                if let Err(e) = self.pvcs().delete(pvc_name, &dp).await {
                    warn!("[K8S] PVC {pvc_name} force-delete(grace=0) 请求失败: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                // 强删后再确认; 仍卡 → 不自动剥 finalizer (危险, 绕过 pvc-protection),
                // 报错让人/运维介入
                match self.pvcs().get(pvc_name).await {
                    Err(kube::Error::Api(ae)) if ae.code == 404 => break,
                    _ => {
                        self.subvolume_path_cache.write().await.remove(pvc_name);
                        return Err(ContainerRuntimeError::K8sError(format!(
                            "PVC '{}' stuck in Terminating after force delete (pvc-protection finalizer?) — manual intervention required",
                            pvc_name
                        )));
                    }
                }
            }
            match self.pvcs().get(pvc_name).await {
                Err(kube::Error::Api(ae)) if ae.code == 404 => {
                    info!(
                        "[K8S] PVC {} fully deleted after {:.1}s",
                        pvc_name,
                        wait_start.elapsed().as_secs_f64()
                    );
                    break;
                }
                Ok(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Err(e) => {
                    // 非 404 错误 (transport/auth/RBAC 瞬时故障): 不假定已删, 继续轮询
                    warn!(
                        "[K8S] PVC {} get failed during wait, keep polling: {}",
                        pvc_name, e
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }

        // invalidate subvolPath cache (destroy 打破 resolve_subvolume_path 的 "PVC 不可变" 假设;
        // 不清则重建同 app_id 时 resolve 命中脏值 → 读老 subvol → 数据面分裂)
        self.subvolume_path_cache.write().await.remove(pvc_name);
        info!("[K8S] PVC destroyed: {}", pvc_name);
        Ok(())
    }
}
