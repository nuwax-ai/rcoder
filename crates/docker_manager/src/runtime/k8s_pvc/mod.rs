//! Kubernetes PVC (Persistent Volume Claim) 生命周期管理
//!
//! 提供 workspace PVC 的创建、等待绑定、删除和 finalizer 检查等功能。
//! 使用 trait extension 模式为 `KubernetesRuntime` 添加 PVC 操作方法。
//!
//! 拆分（~500 行基线）: trait 定义 + 各入口方法在本模块；ensure/destroy 的
//! 共用核心（terminating 等待/漂移可见/创建重试/强删轮询, workspace 卷与
//! UserApp data 卷共用）在 [`lifecycle`]。

#[cfg(feature = "kubernetes")]
mod lifecycle;

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
