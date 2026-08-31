//! PVC ensure/destroy 核心。ensure 核心（workspace 卷）：存在检查/terminating
//! 等待/SC 漂移可见/创建重试；destroy 核心：幂等删除 + 60s Terminating 等待 →
//! 强删 grace=0（另被 Userapp data 卷存量兜底回收复用）。

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
use std::collections::BTreeMap;
#[cfg(feature = "kubernetes")]
use tracing::{info, warn};

#[cfg(feature = "kubernetes")]
use crate::runtime::kubernetes_runtime::KubernetesRuntime;

#[cfg(feature = "kubernetes")]
impl KubernetesRuntime {
    /// PVC ensure 核心（workspace 卷）：存在检查/terminating
    /// 等待/SC 漂移可见/创建重试。`service_type_label` 仅作为 PVC label 值。
    pub(super) async fn ensure_pvc_core(
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
                // Userapp 域（运行卷/开发卷/data 卷）RWO RBD 单容器独占,
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

    /// PVC destroy 核心（workspace 卷与 Userapp data 卷共用）：幂等删除 +
    /// 60s Terminating 等待 → 强删 grace=0。
    pub(super) async fn destroy_pvc_core(&self, pvc_name: &str) -> ContainerRuntimeResult<()> {
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
