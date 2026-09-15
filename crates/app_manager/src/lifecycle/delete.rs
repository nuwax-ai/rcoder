//! Production compute deletion and resource purge retain application identity.
//! Full lifecycle deletion is coordinated separately by purge_app.

use crate::models::*;
use crate::service::AppService;
use crate::utils::*;
use container_runtime_api::DeploymentStatus;
use tracing::{info, instrument};

impl AppService {
    /// 删除应用（v2 §5.3：默认保留持久存储，purge=true 才清空数据面）。
    #[instrument(skip(self))]
    pub async fn delete_app(
        &self,
        app_id: &str,
        purge: bool,
        expected_resource_version: Option<&str>,
    ) -> AppResult<()> {
        let identity = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("Application identity not found: {app_id}"))
            })?;
        self.delete_app_controlled(
            app_id,
            DeleteAppRequest {
                purge: Some(purge),
                expected_resource_version: expected_resource_version.map(str::to_owned),
                lifecycle_id: None,
                request_id: None,
            },
        )
        .await
    }

    pub async fn delete_app_controlled(
        &self,
        app_id: &str,
        request: DeleteAppRequest,
    ) -> AppResult<()> {
        validate_app_id(app_id)?;
        let release_lock = self.acquire_process_release_lock(app_id).await?;
        let result = async {
            self.metadata
                .validate_request_lifecycle(
                    app_id,
                    &request.user_id,
                    request.lifecycle_id.as_deref(),
                )
                .await?;
            use sha2::Digest as _;
            let fingerprint = hex::encode(sha2::Sha256::digest(
                shared_types::encode_userapp_intent(&request).map_err(|error| {
                    AppOperationError::Backend(format!("Encode delete intent: {error}"))
                })?,
            ));
            let purge = request.purge.unwrap_or(false);
            let kind = if purge {
                shared_types::UserAppOperationKind::PurgeResources
            } else {
                shared_types::UserAppOperationKind::DeleteCompute
            };
            let identity = shared_types::UserAppControlRequest {
                user_id: String::new(),
                lifecycle_id: request.lifecycle_id.clone(),
                request_id: request.request_id.clone(),
            };
            if self
                .replay_control(app_id, &identity, kind, &fingerprint)
                .await?
                .is_some()
            {
                return Ok(());
            }
            // A known lifecycle may already have no compute workload. Still
            // capture typed residual resources; query failures are never absence.
            self.get_lifecycle(app_id, &request.user_id).await?;
            let mut durable = crate::service::OwnedOperation::admit(
                self.metadata.store.clone(),
                shared_types::UserAppAdmission {
                    runtime_policy_on_success: None,
                    command: Some(shared_types::UserAppControlCommand::DeleteResources {
                        purge,
                        expected_resource_version: request.expected_resource_version.clone(),
                    }),
                    app_id: app_id.into(),
                    user_id: String::new(),
                    lifecycle_id: request.lifecycle_id.clone(),
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    request_id: request.request_id.clone(),
                    request_fingerprint: fingerprint,
                    kind,
                    metadata: None,
                },
            )
            .await?;
            let mutation = self
                .execute_resource_deletion(
                    app_id,
                    &request.user_id,
                    purge,
                    request.expected_resource_version.as_deref(),
                    &mut durable,
                    &release_lock,
                )
                .await;
            match mutation {
                Ok(()) => {
                    durable.succeed().await?;
                    release_lock.mark_completed();
                    self.activity.forget_app(app_id);
                }
                Err(error) => {
                    if release_lock.has_unfinished_mutation() {
                        durable.fail(&error).await?;
                    } else {
                        durable.reject_without_mutation(&error).await?;
                    }
                    return Err(error);
                }
            }
            self.invalidate_deploy_cache().await;
            Ok(())
        }
        .await;
        if result.is_ok() || !release_lock.has_unfinished_mutation() {
            release_lock.finish().await?;
        }
        self.remove_unused_process_release_lock(app_id);
        result
    }

    /// Execute an admitted deletion without reacquiring the application lock.
    /// Both initial requests and unclaimed-command recovery use this path.
    pub(crate) async fn execute_resource_deletion(
        &self,
        app_id: &str,
        owner: &str,
        purge: bool,
        expected_resource_version: Option<&str>,
        durable: &mut crate::service::OwnedOperation,
        release_lock: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        durable.bind_lease(release_lock, owner).await?;
        let kind = if purge {
            shared_types::UserAppOperationKind::PurgeResources
        } else {
            shared_types::UserAppOperationKind::DeleteCompute
        };
        let previous = self
            .runtime
            .get_deployment_status(app_id)
            .await
            .map_err(|error| map_runtime_error("Read deployment before deletion", error))?
            .unwrap_or_else(|| DeploymentStatus {
                app_id: app_id.into(),
                ..Default::default()
            });
        // 乐观锁（同 update_app）：expected 不匹配 → 409 Conflict
        if let Some(expected) = expected_resource_version
            && let Some(actual) = &previous.resource_version
            && expected != actual
        {
            let error = AppOperationError::Conflict(format!(
                "resource version mismatch: expected={expected}, actual={actual}"
            ));
            return Err(error);
        }
        // delete/purge 必须与 prepare/activate/confirm/delete-release 串行，避免删除 PVC
        // 时另一个任务仍在写版本包或切换 code。
        let snapshot = self
            .runtime
            .capture_app_deletion(app_id, previous.resource_version.as_deref())
            .await
            .map_err(|error| map_runtime_error("capture app deletion", error))?;
        let dev_deletion = if purge {
            Some(self.capture_dev_deletion(app_id).await?)
        } else {
            None
        };
        info!("[APP] deleting app: {} (purge={})", app_id, purge);

        let mut checkpoint = shared_types::UserAppDeletionCheckpoint {
            stage: shared_types::UserAppDeletionStage::Captured,
            schema_version: 1,
            context: durable.execution_context(),
            kind,
            production: snapshot.clone(),
            development: dev_deletion.as_ref().map(|deletion| deletion.receipt()),
        };
        checkpoint.validate().map_err(AppOperationError::Conflict)?;
        durable
            .checkpoint(
                "deleting_resources",
                serde_json::to_value(&checkpoint).map_err(|error| {
                    AppOperationError::Backend(format!("Encode deletion checkpoint: {error}"))
                })?,
            )
            .await?;
        release_lock.mark_mutating()?;
        // 1. 删除计算面（防护序列与失败对称恢复见 tear_down_compute_plane）
        self.tear_down_compute_plane(app_id, &previous, &snapshot)
            .await?;
        durable
            .deletion_progress(
                &mut checkpoint,
                shared_types::UserAppDeletionStage::ComputeRemoved,
            )
            .await?;

        // 2. purge=true 必须销毁持久存储（K8s: PVC + Ceph subvolume；Docker:
        //    workspace 目录），与 API 的“全部删除”语义一致。仅清空目录却保留 PVC
        //    会继续占用配额，并让成功响应与实际状态不一致。
        //    元数据行**保留**（三档语义：delete/purge 保留行支持误删找回，仅独立
        //    storage/destroy 接口删行）。
        if let Some(dev_deletion) = dev_deletion {
            self.finish_captured_purge(app_id, durable, &mut checkpoint, dev_deletion)
                .await?;
            info!("[APP] persistent storage destroyed: {}", app_id);
        } else {
            info!(
                "[APP] retained persistent storage (pass purge=true to clear): {}",
                app_id
            );
        }

        Ok(())
    }

    /// Remove routes and block wake before deleting captured compute targets.
    /// A multi-resource delete error may follow partial mutation: keep the fence
    /// until reconciliation. Final activity cleanup follows durable success.
    /// The caller holds the application guard; this method never reacquires it.
    pub(crate) async fn tear_down_compute_plane(
        &self,
        app_id: &str,
        _previous: &DeploymentStatus,
        snapshot: &shared_types::AppDeletionSnapshot,
    ) -> AppResult<()> {
        self.unregister_pingora_backends(app_id).await;
        self.activity.mark_wake_blocked(app_id);
        if let Err(error) = self.runtime.delete_app_snapshot(snapshot).await {
            return Err(map_runtime_error(
                &format!("[APP] delete_deployment failed app_id={app_id}"),
                error,
            ));
        }
        Ok(())
    }
}
