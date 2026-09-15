//! Kubernetes PVC (Persistent Volume Claim) 生命周期管理
//!
//! 提供 workspace PVC 的创建、等待绑定、删除和 finalizer 检查等功能。
//! 使用 trait extension 模式为 `KubernetesRuntime` 添加 PVC 操作方法。
//!
//! 拆分（~500 行基线）: trait 定义 + 各入口方法在本模块；ensure/destroy 的
//! 共用核心（terminating 等待/漂移可见/创建重试/强删轮询）在 [`lifecycle`]
//! ——ensure 核心服务 workspace 卷；destroy 核心另被 data 卷兜底回收复用。

#[cfg(feature = "kubernetes")]
mod lifecycle;

/// 默认 PVC 存储大小（当请求未指定 storage_size 时使用）
///
/// agent/computer 域的 CephFS 卷是共享数据目录，**不构成业务容量约束**
/// （真实上限是 CephFS 池容量）——此 50Gi 仅是 PVC 对象 spec 必填的惯性
/// 兜底值，作为「接口入参 resource_limits.storage_size」与「config.yml
/// resource_limits.storage_size」均未指定时的最终兜底。磁盘空间限制语义
/// 仅存在于 Userapp 域独立 RBD 卷（见 [`DEFAULT_USERAPP_PVC_STORAGE_SIZE`]）。
#[cfg(feature = "kubernetes")]
const DEFAULT_PVC_STORAGE_SIZE: &str = "50Gi";

/// Userapp 域（生产运行卷 + builder 开发卷）的默认容量兜底——per-app RBD
/// 块设备独占，是唯一有真实磁盘空间限制语义的卷型（RBD image size 即硬
/// 上限）；仅当 create 请求未带 `resources.storage` / builder ensure 未
/// 显式传 size 时生效（已存在 PVC 复用旧值，不自动扩容）。
#[cfg(feature = "kubernetes")]
const DEFAULT_USERAPP_PVC_STORAGE_SIZE: &str = "100Gi";

#[cfg(feature = "kubernetes")]
use async_trait::async_trait;
#[cfg(feature = "kubernetes")]
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult, StorageResizeOutcome};
#[cfg(feature = "kubernetes")]
use shared_types::ServiceType;
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
/// - Userapp PVC 容量调整 (`resize_app_pvc`, 只扩不缩)
///
/// - PVC 销毁 (`destroy_workspace_pvc`, 仅 Userapp 经 REST `storage/destroy` 显式调用)
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

    /// Userapp 生产运行容器历史第二块数据卷（`-data` 后缀）的卷名。
    ///
    /// 格式：`{container_prefix}-{sanitized_id}-data`。**已随单卷四 subPath 化退役**
    /// （prod 卷内 `{app_id}/ data/ logs/ agent-store/` 四目录平级，挂载在
    /// workspace 卷上）——仅 destroy_app_pvc 兜底回收存量旧 PVC 时使用。
    fn app_data_pvc_name(&self, app_id: &str) -> ContainerRuntimeResult<String>;

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

    /// 调整 Userapp per-app 运行卷容量（`WorkspaceRuntime::resize_app_storage` 的 K8s 内核）。
    ///
    /// 读 PVC 当前 `requests.storage` → quantity 归一比较：等量 no-op；更大 merge
    /// patch 扩容（external-resizer 异步生效，在线扩文件系统不重建 Pod）；更小
    /// [`StorageResizeOutcome::ShrinkRejected`] 事实上抛（错误决策归 app_manager 域层）。
    /// 前提：StorageClass `allowVolumeExpansion=true`，否则 patch 被接受但静默不生效。
    async fn resize_app_pvc(
        &self,
        app_id: &str,
        new_size: &str,
    ) -> ContainerRuntimeResult<StorageResizeOutcome>;

    /// 销毁 workspace PVC + CephFS subvolume (释放配额, 不可逆)。
    ///
    /// 仅 Userapp 经 REST `POST /apps/{id}/storage/destroy` 显式调用 (agent PVC 永不删)。
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

/// Userapp 域（生产运行卷 + builder 开发卷）的 storage class（env
/// `RCODER_USERAPP_STORAGE_CLASS` 显式覆盖，兼容回退旧名
/// `RCODER_USERAPP_BUILDER_STORAGE_CLASS`；缺省 None = PVC 不指定
/// storageClassName → 集群 default StorageClass，如 229/19 的 `ceph-rbd`
/// ——Ceph RBD 块存储）。Userapp 两类卷均 RWO 单容器独占：rcoder 不挂卷
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
        // Userapp 域（生产运行卷 + builder 开发卷）: RWO + RBD（env 可覆盖）,
        // 兜底容量 100Gi; 其余 service_type 用全局 access_mode/storage_class
        // 与 50Gi 兜底。
        let (access_mode, storage_class_name, default_size) = match service_type {
            ServiceType::Userapp | ServiceType::UserappBuilder => (
                "ReadWriteOnce".to_string(),
                userapp_storage_class(),
                DEFAULT_USERAPP_PVC_STORAGE_SIZE,
            ),
            _ => (
                self.config.access_mode.clone(),
                Some(self.config.storage_class.clone()),
                DEFAULT_PVC_STORAGE_SIZE,
            ),
        };
        let mut extra_labels: Vec<(String, String)> = Vec::new();
        // builder 聚合标签（identifier 应用共享后即纯 app_id；存量复合键
        // 残留右切还原，保按 app 聚合清理可用）。
        if matches!(service_type, ServiceType::UserappBuilder) {
            extra_labels.push(("rcoder.io/identifier".to_string(), identifier.to_string()));
            extra_labels.push((
                "rcoder.io/app-id".to_string(),
                shared_types::legacy_composite_app_segment(identifier).to_string(),
            ));
        }
        self.ensure_pvc_core(
            &pvc_name,
            &service_type.to_string(),
            access_mode,
            storage_class_name,
            storage_size.unwrap_or(default_size),
            None,
            &extra_labels,
        )
        .await
    }

    fn app_data_pvc_name(&self, app_id: &str) -> ContainerRuntimeResult<String> {
        let prefix = KubernetesRuntime::sanitize_k8s_name_part(
            &self.service_container_prefix(&ServiceType::Userapp)?,
        );
        let sanitized = app_id.replace('_', "-");
        Ok(format!("{}-{}-data", prefix, sanitized))
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

    async fn resize_app_pvc(
        &self,
        app_id: &str,
        new_size: &str,
    ) -> ContainerRuntimeResult<StorageResizeOutcome> {
        let pvc_name = self.workspace_pvc_name(app_id, &ServiceType::Userapp)?;
        let pvc = self.pvcs().get(&pvc_name).await.map_err(|e| match e {
            kube::Error::Api(ae) if ae.code == 404 => ContainerRuntimeError::ContainerNotFound(
                format!("PVC '{pvc_name}' for app {app_id} not found"),
            ),
            other => {
                ContainerRuntimeError::K8sError(format!("Failed to get PVC '{pvc_name}': {other}"))
            }
        })?;
        let identity = pvc_resize_identity(&pvc_name, &pvc.metadata)?;
        let current = pvc
            .spec
            .as_ref()
            .and_then(|s| s.resources.as_ref())
            .and_then(|r| r.requests.as_ref())
            .and_then(|r| r.get("storage"))
            .map(|q| q.0.clone()) // Quantity(newtype) → 内部字符串
            .ok_or_else(|| {
                ContainerRuntimeError::K8sError(format!(
                    "PVC '{pvc_name}' has no spec.resources.requests.storage"
                ))
            })?;
        self.resize_captured_pvc(&identity, &current, new_size)
            .await
    }

    async fn destroy_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        if !matches!(
            service_type,
            ServiceType::Userapp | ServiceType::UserappBuilder
        ) {
            return Err(ContainerRuntimeError::ConfigurationError(
                "agent workspace PVC deletion is forbidden".into(),
            ));
        }
        // UserappBuilder 现为 per-app RWO PVC（app 删除 purge 时随容器一并回收,
        // 调用方为 Userapp 域 REST 流程, 符合"agent PVC 永不删"约束的例外面）。
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;
        self.destroy_pvc_core(&pvc_name).await
    }
}

impl KubernetesRuntime {
    pub(super) async fn ensure_owned_workspace_pvc(
        &self,
        context: &shared_types::UserAppExecutionContext,
        service_type: &ServiceType,
        storage_size: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if !matches!(
            service_type,
            ServiceType::Userapp | ServiceType::UserappBuilder
        ) {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Lifecycle-owned PVC requires UserApp family".into(),
            ));
        }
        let name = self.workspace_pvc_name(&context.app_id, service_type)?;
        self.ensure_pvc_core(
            &name,
            &service_type.to_string(),
            "ReadWriteOnce".into(),
            userapp_storage_class(),
            storage_size.unwrap_or(DEFAULT_USERAPP_PVC_STORAGE_SIZE),
            Some(context),
            &[],
        )
        .await
    }

    pub(super) async fn capture_storage_resize(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::UserAppStorageResizeTarget> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let name = self.workspace_pvc_name(&context.app_id, &ServiceType::Userapp)?;
        let pvc = self.pvcs().get(&name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Capture application storage resize: {error}"))
        })?;
        let resource = pvc_resize_identity(&name, &pvc.metadata)?;
        let metadata = pvc.metadata.annotations.as_ref().ok_or_else(|| {
            ContainerRuntimeError::Conflict(
                "Application storage requires lifecycle adoption".into(),
            )
        })?;
        context
            .validate_application_metadata(metadata)
            .map_err(ContainerRuntimeError::Conflict)?;
        let current_size = pvc
            .spec
            .as_ref()
            .and_then(|spec| spec.resources.as_ref())
            .and_then(|resources| resources.requests.as_ref())
            .and_then(|requests| requests.get("storage"))
            .map(|value| value.0.clone())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "Application storage has no requested capacity".into(),
                )
            })?;
        Ok(shared_types::UserAppStorageResizeTarget {
            context: context.clone(),
            resource,
            current_size,
        })
    }

    pub(super) async fn resize_storage_target(
        &self,
        target: &shared_types::UserAppStorageResizeTarget,
        new_size: &str,
    ) -> ContainerRuntimeResult<StorageResizeOutcome> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let expected = self.workspace_pvc_name(&target.context.app_id, &ServiceType::Userapp)?;
        if target.resource.name != expected
            || target.resource.kind != shared_types::AppResourceKind::PersistentVolumeClaim
            || target.resource.uid.is_empty()
            || target
                .resource
                .resource_version
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Incomplete captured storage resize identity".into(),
            ));
        }
        self.resize_captured_pvc(&target.resource, &target.current_size, new_size)
            .await
    }

    async fn resize_captured_pvc(
        &self,
        identity: &shared_types::AppResourceIdentity,
        current: &str,
        new_size: &str,
    ) -> ContainerRuntimeResult<StorageResizeOutcome> {
        let pvc_name = &identity.name;
        let current = current.to_owned();
        match compare_storage_quantity(&current, new_size) {
            StorageVerdict::Equal => {
                info!(
                    "[K8S] PVC {} resize no-op: requested {} equals current {}",
                    pvc_name, new_size, current
                );
                Ok(StorageResizeOutcome::AlreadyEqual)
            }
            StorageVerdict::Shrink => {
                // Ok 载事实（K8s 层不做错误决策）；app_manager 收到后转 400
                info!(
                    "[K8S] PVC {} resize rejected: requested {} < current {} (PVC shrinking is unsupported)",
                    pvc_name, new_size, current
                );
                Ok(StorageResizeOutcome::ShrinkRejected {
                    requested: new_size.to_string(),
                    current,
                })
            }
            StorageVerdict::Grow => {
                // merge patch requests.storage → external-resizer 异步扩容（在线，
                // kubelet 扩文件系统不重建 Pod）；SC 须 allowVolumeExpansion=true
                let patch = serde_json::json!({
                    "metadata": {"uid": identity.uid, "resourceVersion": identity.resource_version},
                    "spec": { "resources": { "requests": { "storage": new_size } } }
                });
                self.pvcs()
                    .patch(
                        pvc_name,
                        &kube::api::PatchParams::default(),
                        &kube::api::Patch::Merge(&patch),
                    )
                    .await
                    .map_err(|error| match &error {
                        kube::Error::Api(response) if response.code == 409 => ContainerRuntimeError::Conflict(format!("PVC resize precondition failed for {pvc_name}: {error}")),
                        _ => ContainerRuntimeError::K8sError(format!("Failed to patch PVC '{pvc_name}' requests.storage to {new_size}: {error}")),
                    })?;
                info!(
                    "[K8S] PVC {} resize requested: {} -> {} (external-resizer async, online)",
                    pvc_name, current, new_size
                );
                Ok(StorageResizeOutcome::Resized {
                    from: current,
                    to: new_size.to_string(),
                })
            }
            StorageVerdict::Invalid => Err(ContainerRuntimeError::ConfigurationError(format!(
                "invalid storage quantity (current={current:?}, requested={new_size:?}); \
                 expected K8s Quantity format like \"100Gi\""
            ))),
        }
    }
}

/// Capture a production PVC identity even for no-op size requests. A name or
/// matching capacity cannot establish resource ownership.
#[cfg(feature = "kubernetes")]
fn pvc_resize_identity(
    expected_name: &str,
    metadata: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> ContainerRuntimeResult<shared_types::AppResourceIdentity> {
    if metadata.name.as_deref() != Some(expected_name) || metadata.deletion_timestamp.is_some() {
        return Err(ContainerRuntimeError::Conflict(
            "PVC resize target changed or is deleting".into(),
        ));
    }
    if metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get("service_type"))
        .map(String::as_str)
        != Some(ServiceType::Userapp.to_string().as_str())
    {
        return Err(ContainerRuntimeError::Conflict(
            "PVC resize target is not a production UserApp volume".into(),
        ));
    }
    let uid = metadata
        .uid
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError("PVC resize target has no UID".into())
        })?;
    let version = metadata
        .resource_version
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "PVC resize target has no resource version".into(),
            )
        })?;
    Ok(shared_types::AppResourceIdentity {
        kind: shared_types::AppResourceKind::PersistentVolumeClaim,
        name: expected_name.into(),
        uid: uid.into(),
        resource_version: Some(version.into()),
    })
}

/// quantity 归一比较结果（`resize_app_pvc` 决策用）。
#[cfg(feature = "kubernetes")]
#[derive(Debug, PartialEq, Eq)]
enum StorageVerdict {
    Grow,
    Equal,
    Shrink,
    Invalid,
}

/// 比较两个 K8s storage quantity（`parse_memory_quantity` 归一为字节数）。
/// 单位混写等量（"100Gi" vs "102400Mi" / 纯数字字节）判 Equal；
/// 任一非法格式判 Invalid（防 stored PVC 上的怪值静默走进 patch）。
#[cfg(feature = "kubernetes")]
fn compare_storage_quantity(current: &str, requested: &str) -> StorageVerdict {
    match (
        shared_types::parse_memory_quantity(current),
        shared_types::parse_memory_quantity(requested),
    ) {
        (Some(cur), Some(req)) if req > cur => StorageVerdict::Grow,
        (Some(cur), Some(req)) if req == cur => StorageVerdict::Equal,
        (Some(_), Some(_)) => StorageVerdict::Shrink,
        _ => StorageVerdict::Invalid,
    }
}

#[cfg(all(test, feature = "kubernetes"))]
mod tests {
    use super::*;

    #[test]
    fn pvc_resize_identity_rejects_replacement_family_and_missing_preconditions() {
        let original = k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some("pvc-original".into()),
            uid: Some("uid-original".into()),
            resource_version: Some("47".into()),
            labels: Some(std::collections::BTreeMap::from([(
                "service_type".into(),
                ServiceType::Userapp.to_string(),
            )])),
            ..Default::default()
        };
        let identity = pvc_resize_identity("pvc-original", &original).expect("identity");
        assert_eq!(identity.uid, "uid-original");
        assert_eq!(identity.resource_version.as_deref(), Some("47"));
        assert!(pvc_resize_identity("other-name", &original).is_err());
        let mut foreign = original.clone();
        foreign.labels.as_mut().expect("labels").insert(
            "service_type".into(),
            ServiceType::UserappBuilder.to_string(),
        );
        assert!(pvc_resize_identity("pvc-original", &foreign).is_err());
        let mut missing = original.clone();
        missing.uid = None;
        assert!(pvc_resize_identity("pvc-original", &missing).is_err());
        let mut missing = original;
        missing.resource_version = None;
        assert!(pvc_resize_identity("pvc-original", &missing).is_err());
    }

    #[test]
    fn compare_storage_quantity_verdicts() {
        assert_eq!(
            compare_storage_quantity("50Gi", "100Gi"),
            StorageVerdict::Grow
        );
        assert_eq!(
            compare_storage_quantity("100Gi", "50Gi"),
            StorageVerdict::Shrink
        );
        assert_eq!(
            compare_storage_quantity("100Gi", "100Gi"),
            StorageVerdict::Equal
        );
        // 单位混写等量：归一为字节数后比较（100Gi = 102400Mi = 107374182400B）
        assert_eq!(
            compare_storage_quantity("100Gi", "102400Mi"),
            StorageVerdict::Equal
        );
        assert_eq!(
            compare_storage_quantity("107374182400", "100Gi"),
            StorageVerdict::Equal
        );
        // 十进制 vs 二进制：100G(1e11) < 100Gi(≈1.07e11) → 判缩容
        assert_eq!(
            compare_storage_quantity("100Gi", "100G"),
            StorageVerdict::Shrink
        );
        // 非法格式（未识别后缀 / 负数 / 空串）→ Invalid，不静默 patch
        assert_eq!(
            compare_storage_quantity("100XX", "200Gi"),
            StorageVerdict::Invalid
        );
        assert_eq!(
            compare_storage_quantity("100Gi", "-5Gi"),
            StorageVerdict::Invalid
        );
        assert_eq!(
            compare_storage_quantity("", "100Gi"),
            StorageVerdict::Invalid
        );
    }
}
