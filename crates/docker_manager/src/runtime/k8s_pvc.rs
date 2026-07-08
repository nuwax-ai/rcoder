//! Kubernetes PVC (Persistent Volume Claim) 生命周期管理
//!
//! 提供 workspace PVC 的创建、等待绑定、删除和 finalizer 检查等功能。
//! 使用 trait extension 模式为 `KubernetesRuntime` 添加 PVC 操作方法。

/// 默认 PVC 存储大小（当请求未指定 storage_size 时使用）
///
/// 10Gi 配合 cephfs 共享存储，作为「接口入参 resource_limits.storage_size」与
/// 「config.yml resource_limits.storage_size」均未指定时的最终兜底
/// （详见 K8sPvcOps::ensure_workspace_pvc）。
#[cfg(feature = "kubernetes")]
const DEFAULT_PVC_STORAGE_SIZE: &str = "10Gi";

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

        // Check if PVC already exists and its state
        let pvc_status = match self.pvcs().get(&pvc_name).await {
            Ok(pvc) => {
                if pvc.metadata.deletion_timestamp.is_some() {
                    // PVC is in Terminating state — it's being deleted.
                    // We must wait for it to be fully removed before creating a new one,
                    // otherwise the new pod will reference a PVC that's about to disappear.
                    info!(
                        "[K8S] PVC {} is in Terminating state, waiting for deletion to complete...",
                        pvc_name
                    );
                    "terminating"
                } else {
                    info!("[K8S] PVC {} already exists and is active", pvc_name);
                    "active"
                }
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => "not_found",
            Err(e) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Failed to check PVC '{}': {}",
                    pvc_name, e
                )));
            }
        };

        match pvc_status {
            "active" => {
                // PVC exists and is not being deleted — reuse it.
                // WaitForFirstConsumer PVCs will be Bound once a Pod referencing them is scheduled.
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
                    match self.pvcs().get(&pvc_name).await {
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
                                let _ = self.pvcs().delete(&pvc_name, &dp).await;
                                // Wait a bit more for the API to reflect the deletion
                                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        Err(_) => {
                            // On error, assume PVC is gone
                            break;
                        }
                    }
                }
            }
            _ => {
                // "not_found" — PVC doesn't exist, will create below
            }
        }
        // If not found or terminated, create it (falls through to creation logic below)

        let storage_size = storage_size.unwrap_or(DEFAULT_PVC_STORAGE_SIZE);

        let pvc = PersistentVolumeClaim {
            metadata: ObjectMeta {
                name: Some(pvc_name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some({
                    let mut m = BTreeMap::new();
                    m.insert("app".to_string(), "rcoder".to_string());
                    m.insert("app.kubernetes.io/managed-by".to_string(), "rcoder-runtime".to_string());
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
}
