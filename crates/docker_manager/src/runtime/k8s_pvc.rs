//! Kubernetes PVC (Persistent Volume Claim) 生命周期管理
//!
//! 提供 workspace PVC 的创建、等待绑定、删除和 finalizer 检查等功能。
//! 使用 trait extension 模式为 `KubernetesRuntime` 添加 PVC 操作方法。

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
use kube::api::{DeleteParams, ObjectMeta, PostParams};
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
/// - PVC 删除 (`delete_workspace_pvc`)
/// - PVC finalizer 检查 (`wait_for_pvc_removable`)
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

    /// 等待 PVC 进入 Bound 状态
    ///
    /// 保留用于 WaitForFirstConsumer 模式下切换为预绑定策略时使用。
    #[allow(dead_code)]
    async fn wait_for_pvc_bound(&self, pvc_name: &str) -> ContainerRuntimeResult<()>;

    /// 删除 workspace PVC
    ///
    /// 调用前应先确保 Pod 已完全终止（通过 `wait_for_pod_terminated`），
    /// 且 PVC 的 `pvc-protection` finalizer 已被移除（通过 `wait_for_pvc_removable`）。
    async fn delete_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()>;

    /// 等待 PVC 的 `pvc-protection` finalizer 被移除，使其可安全删除
    ///
    /// Pod 终止后，kubelet 卸载卷 → PVC controller 移除 finalizer，
    /// 这个过程通常只需几秒，但 FUSE 卷可能因 unmount 缓慢而延迟。
    /// 等待最多 15s，超时后仍允许调用方尝试删除（可能失败但不阻塞流程）。
    async fn wait_for_pvc_removable(&self, pvc_name: &str) -> ContainerRuntimeResult<()>;
}

#[cfg(feature = "kubernetes")]
#[async_trait]
impl K8sPvcOps for KubernetesRuntime {
    fn workspace_pvc_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        let prefix =
            KubernetesRuntime::sanitize_k8s_name_part(&self.service_container_prefix(service_type)?);
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
            info!(
                "[K8S] PVC {} already exists, skipping Bound check (WaitForFirstConsumer)",
                pvc_name
            );
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

        self.pvcs()
            .create(&PostParams::default(), &pvc)
            .await
            .map_err(|e| {
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

    async fn delete_workspace_pvc(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        let pvc_name = self.workspace_pvc_name(identifier, service_type)?;

        match self
            .pvcs()
            .delete(&pvc_name, &DeleteParams::default())
            .await
        {
            Ok(_) => {
                info!("[K8S] PVC {} delete requested successfully", pvc_name);
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                debug!("[K8S] PVC {} not found, already deleted", pvc_name);
            }
            Err(kube::Error::Api(ae)) if ae.code == 409 => {
                // PVC 仍有 finalizer（pvc-protection），删除请求已被 K8s 接受但会等待
                // 这种情况不应发生（调用前应先等待 finalizer 移除），记录日志便于排查
                info!(
                    "[K8S] PVC {} has active finalizers (409 Conflict), delete will be deferred",
                    pvc_name
                );
            }
            Err(e) => {
                warn!("[K8S] Failed to delete PVC '{}': {}", pvc_name, e);
            }
        }
        Ok(())
    }

    async fn wait_for_pvc_removable(&self, pvc_name: &str) -> ContainerRuntimeResult<()> {
        let timeout = std::time::Duration::from_secs(15);
        let poll_interval = std::time::Duration::from_secs(1);
        let start = std::time::Instant::now();

        while start.elapsed() < timeout {
            match self.pvcs().get(pvc_name).await {
                Ok(pvc) => {
                    let has_protection_finalizer = pvc
                        .metadata
                        .finalizers
                        .as_ref()
                        .map(|finalizers| {
                            finalizers
                                .iter()
                                .any(|f| f == "kubernetes.io/pvc-protection")
                        })
                        .unwrap_or(false);

                    if !has_protection_finalizer {
                        info!(
                            "[K8S] PVC {} protection finalizer removed ({:.1}s), safe to delete",
                            pvc_name,
                            start.elapsed().as_secs_f64()
                        );
                        return Ok(());
                    }

                    debug!(
                        "[K8S] PVC {} still has pvc-protection finalizer, waiting ({:.0}s)...",
                        pvc_name,
                        start.elapsed().as_secs_f64()
                    );
                }
                Err(kube::Error::Api(ae)) if ae.code == 404 => {
                    // PVC 已被自动删除（StorageClass 的 ReclaimPolicy=Delete 触发）
                    info!("[K8S] PVC {} already deleted (404)", pvc_name);
                    return Ok(());
                }
                Err(e) => {
                    debug!(
                        "[K8S] Poll PVC {} returned {} (will retry)",
                        pvc_name, e
                    );
                }
            }
            tokio::time::sleep(poll_interval).await;
        }

        warn!(
            "[K8S] PVC {} pvc-protection finalizer not removed in 15s, will attempt delete anyway",
            pvc_name
        );
        Ok(())
    }
}
