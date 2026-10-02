use super::*;

impl crate::service::AppService {
    /// 销毁应用持久存储 PVC（高危·不可逆·释放配额）。
    ///
    /// - `prod`：安全约束 ① 仅当 app 计算资源已不存在（已 delete）时允许（否则
    ///   INVALID_STATE）；② body `confirm` 必须等于 app_id。K8s 删 PVC 对象，
    ///   Docker 等价删 bind 目录；保留应用生命周期和元数据。
    /// - `dev`：销毁**整个开发环境** = UserappDevCleanup 四步回收（builder 容器 +
    ///   dev PVC + Docker dev 目录 + 摘注册/探活缓存）；**不动 metadata**（owner
    ///   保留——create-workspace 幂等重建开发环境）。幂等：资源不存在视为成功。
    pub async fn destroy_app_storage(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        confirm: &str,
    ) -> AppResult<()> {
        self.destroy_app_storage_controlled(
            app_stage,
            app_id,
            DestroyStorageRequest {
                confirm: confirm.into(),
                lifecycle_id: None,
                request_id: None,
            },
        )
        .await
        .map(|_| ())
    }

    pub async fn destroy_app_storage_controlled(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        request: DestroyStorageRequest,
    ) -> AppResult<String> {
        use garde::Validate as _;
        use sha2::Digest as _;
        validate_app_id(app_id)?;
        request
            .validate()
            .map_err(|error| AppOperationError::Validation(error.to_string()))?;
        if request.confirm != app_id {
            return Err(AppOperationError::Validation(
                "confirm must equal app_id for destroy".into(),
            ));
        }
        let mut guard = self
            .acquire_process_release_lock_scoped(
                app_id,
                if app_stage == UserappStage::Prod {
                    shared_types::UserAppOperationScope::Prod
                } else {
                    shared_types::UserAppOperationScope::Dev
                },
            )
            .await?;
        let result = async {
            self.metadata
                .validate_request_lifecycle(app_id, request.lifecycle_id.as_deref())
                .await?;
            self.get_lifecycle(app_id).await?;
            let production = app_stage == UserappStage::Prod;
            let command = shared_types::UserAppControlCommand::DestroyStorage { production };
            let fingerprint = hex::encode(sha2::Sha256::digest(
                shared_types::encode_userapp_intent(
                    &serde_json::json!({"stage":app_stage.as_str(), "request":request}),
                )
                .map_err(|error| {
                    AppOperationError::Backend(format!(
                        "Encode storage destruction intent: {error}"
                    ))
                })?,
            ));
            let control = shared_types::UserAppControlRequest {
                lifecycle_id: request.lifecycle_id.clone(),
                request_id: request.request_id.clone(),
            };
            if let Some(existing) = self
                .replay_control(app_id, &control, command.kind(), &fingerprint)
                .await?
            {
                return Ok(existing.operation_id);
            }
            let operation_id = uuid::Uuid::new_v4().to_string();
            let mut operation = crate::service::OwnedOperation::admit(
                self.metadata.store.clone(),
                shared_types::UserAppAdmission {
                    runtime_policy_on_success: None,
                    metadata: None,
                    kind: command.kind(),
                    command: Some(command),
                    app_id: app_id.into(),
                    lifecycle_id: request.lifecycle_id,
                    request_id: request.request_id,
                    operation_id: operation_id.clone(),
                    request_fingerprint: fingerprint,
                },
            )
            .await?;
            match self
                .execute_storage_destruction(app_id, production, &mut operation, &mut guard)
                .await
            {
                Ok(()) => {
                    operation.succeed().await?;
                    guard.mark_completed();
                    self.invalidate_deploy_cache().await;
                    Ok(operation_id)
                }
                Err(error) => {
                    if guard.has_unfinished_mutation() {
                        operation.fail(&error).await?;
                    } else {
                        operation.reject_without_mutation(&error).await?;
                    }
                    Err(error)
                }
            }
        }
        .await;
        if result.is_ok() || !guard.has_unfinished_mutation() {
            guard.finish().await?;
        }
        result
    }

    pub(crate) async fn execute_storage_destruction(
        &self,
        app_id: &str,
        production: bool,
        operation: &mut crate::service::OwnedOperation,
        guard: &mut crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        operation.bind_lease(guard).await?;
        if production {
            self.ensure_app_deleted(app_id, "destroying captured storage")
                .await?;
        }
        let development = self
            .capture_dev_deletion_with_lease(
                app_id,
                if production {
                    None
                } else {
                    Some(guard.builder_lease()?)
                },
            )
            .await?;
        let snapshot = if production {
            Some(
                self.runtime
                    .capture_app_deletion(app_id, None)
                    .await
                    .map_err(|error| map_runtime_error("Capture storage destruction", error))?,
            )
        } else {
            None
        };
        let evidence = shared_types::UserAppStorageDestruction {
            context: operation.execution_context(),
            production: snapshot,
            development: development.receipt(),
        };
        evidence.validate().map_err(AppOperationError::Conflict)?;
        let checkpoint = serde_json::to_value(&evidence).map_err(|error| {
            AppOperationError::Backend(format!("Encode storage destruction checkpoint: {error}"))
        })?;
        operation
            .checkpoint("storage_captured", checkpoint.clone())
            .await?;
        operation.authorize_mutation().await?;
        guard.mark_mutating()?;
        if let Some(snapshot) = &evidence.production {
            self.ensure_app_deleted(app_id, "destroying captured storage")
                .await?;
            self.runtime
                .destroy_app_storage_snapshot(snapshot)
                .await
                .map_err(|error| map_runtime_error("Destroy captured storage", error))?;
            operation
                .checkpoint("production_storage_removed", checkpoint.clone())
                .await?;
        }
        development.cleanup().await.map_err(|error| {
            AppOperationError::Backend(format!("Destroy captured development storage: {error}"))
        })?;
        operation
            .checkpoint("development_storage_removed", checkpoint)
            .await?;
        Ok(())
    }

    pub(crate) async fn capture_dev_deletion(
        &self,
        app_id: &str,
    ) -> AppResult<Box<dyn shared_types::UserappDevDeletion>> {
        self.capture_dev_deletion_with_lease(app_id, None).await
    }

    async fn capture_dev_deletion_with_lease(
        &self,
        app_id: &str,
        lease: Option<Box<dyn shared_types::AppOperationLease>>,
    ) -> AppResult<Box<dyn shared_types::UserappDevDeletion>> {
        let cleanup = self
            .dev_cleanup
            .read()
            .map_err(|_| AppOperationError::Backend("dev cleanup lock poisoned".into()))?
            .clone()
            .ok_or_else(|| AppOperationError::Backend("userapp dev cleanup not injected".into()))?;
        let deletion = match lease {
            Some(lease) => cleanup.capture_with_lease(app_id, lease).await,
            None => cleanup.capture(app_id).await,
        }
        .map_err(|error| {
            AppOperationError::Backend(format!("capture userapp dev deletion: {error}"))
        })?;
        let receipt = deletion.receipt();
        // snapshot 的 app identifier 已是纯 app_id（用户绑定移除，无复合键）
        if receipt.runtime.app_id != app_id {
            return Err(AppOperationError::Conflict(
                "Captured development deletion belongs to another application".into(),
            ));
        }
        Ok(deletion)
    }

    /// 销毁持久存储但**保留业务元数据行**（delete_app 的 purge 分支专用）。
    ///
    /// Production purge and standalone storage destruction retain identity.
    /// Only full delete/app terminates the durable application lifecycle.
    /// Full deletion ends the lifecycle only after all captured resources are removed.
    /// A failed or cancelled execution retains its durable operation for recovery.
    pub async fn purge_app(&self, app_id: &str) -> AppResult<()> {
        let _app = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("Application identity not found: {app_id}"))
            })?;
        self.purge_app_controlled(
            app_id,
            shared_types::UserAppControlRequest {
                lifecycle_id: None,
                request_id: None,
            },
        )
        .await
    }

    pub async fn purge_app_controlled(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
    ) -> AppResult<()> {
        validate_app_id(app_id)?;
        let release_lock = self.acquire_process_release_lock(app_id).await?;
        let result = self
            .purge_app_admitted(app_id, request, &release_lock)
            .await;
        if result.is_ok() || !release_lock.has_unfinished_mutation() {
            release_lock.finish().await?;
        }
        result
    }

    async fn purge_app_admitted(
        &self,
        app_id: &str,
        request: shared_types::UserAppControlRequest,
        release_lock: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        // Read identity after acquiring the operation guard; a previous lifecycle
        // must never authorize deleting resources created while this caller waited.
        let app = self.get_lifecycle(app_id).await?;
        if request
            .lifecycle_id
            .as_ref()
            .is_some_and(|id| id != &app.lifecycle_id)
            || (app.lifecycle_epoch > 1 && request.lifecycle_id.is_none())
        {
            return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
        }
        use sha2::Digest as _;
        let fingerprint = hex::encode(sha2::Sha256::digest(
            shared_types::encode_userapp_intent(&request).map_err(|error| {
                AppOperationError::Backend(format!("Encode application deletion intent: {error}"))
            })?,
        ));
        if self
            .replay_control(
                app_id,
                &request,
                shared_types::UserAppOperationKind::DeleteApplication,
                &fingerprint,
            )
            .await?
            .is_some()
        {
            return Ok(());
        }
        if app.state == shared_types::UserAppLifecycleState::Deleted {
            return Ok(());
        }
        let mut operation = crate::service::OwnedOperation::admit(
            self.metadata.store.clone(),
            shared_types::UserAppAdmission {
                runtime_policy_on_success: None,
                command: Some(shared_types::UserAppControlCommand::DeleteApplication),
                metadata: None,
                app_id: app_id.into(),
                lifecycle_id: Some(app.lifecycle_id),
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: request.request_id,
                request_fingerprint: fingerprint,
                kind: shared_types::UserAppOperationKind::DeleteApplication,
            },
        )
        .await?;
        match self
            .purge_app_resources(app_id, &mut operation, release_lock)
            .await
        {
            Ok(()) => {
                let completed_lifecycle = operation.execution_context().lifecycle_id;
                operation.succeed().await?;
                release_lock.mark_completed();
                self.activity.forget_lifecycle(app_id, &completed_lifecycle);
                self.invalidate_deploy_cache().await;
                Ok(())
            }
            Err(error) => {
                operation.fail(&error).await.map_err(|persist| AppOperationError::Backend(
                    format!("Application deletion failed: {error}; recording failure also failed: {persist}")
                ))?;
                Err(error)
            }
        }
    }

    pub(crate) async fn purge_app_resources(
        &self,
        app_id: &str,
        operation: &mut crate::service::OwnedOperation,
        release_lock: &crate::service::AppOperationGuard,
    ) -> AppResult<()> {
        operation.bind_lease(release_lock).await?;
        // 与发布链（prepare/activate/confirm/delete-release）及 create/update/delete
        // 串行：purge 全程删计算+存储，不能与写版本包/切 code 并发。
        let dev_deletion = self.capture_dev_deletion(app_id).await?;
        let snapshot = self
            .runtime
            .capture_app_deletion(app_id, None)
            .await
            .map_err(|e| map_runtime_error("capture purge resources", e))?;
        let mut checkpoint = shared_types::UserAppDeletionCheckpoint {
            stage: shared_types::UserAppDeletionStage::Captured,
            schema_version: 1,
            context: operation.execution_context(),
            kind: shared_types::UserAppOperationKind::DeleteApplication,
            production: snapshot.clone(),
            development: Some(dev_deletion.receipt()),
        };
        checkpoint.validate().map_err(AppOperationError::Conflict)?;
        operation
            .checkpoint(
                "resources_captured",
                serde_json::to_value(&checkpoint).map_err(|error| {
                    AppOperationError::Backend(format!("Encode deletion checkpoint: {error}"))
                })?,
            )
            .await?;
        // 1. prod 计算面：存在才拆（防护序列 + 失败对称恢复见 tear_down_compute_plane）
        operation.authorize_mutation().await?;
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(previous)) => {
                operation.authorize_mutation().await?;
                release_lock.mark_mutating()?;
                self.tear_down_compute_plane(app_id, &previous, &snapshot)
                    .await?
            }
            Ok(None) => {
                if snapshot.resources.iter().any(|resource| {
                    resource.kind != shared_types::AppResourceKind::PersistentVolumeClaim
                }) {
                    operation.authorize_mutation().await?;
                    release_lock.mark_mutating()?;
                    self.tear_down_compute_plane(
                        app_id,
                        &container_runtime_api::DeploymentStatus {
                            app_id: app_id.into(),
                            ..Default::default()
                        },
                        &snapshot,
                    )
                    .await?;
                }
                info!(
                    "[APP] purge: captured compute resources are absent: {}",
                    app_id
                );
            }
            Err(e) => {
                warn!(
                    "[APP] purge: query app status failed app_id={}: {}",
                    app_id, e
                );
                return Err(AppOperationError::Backend(format!(
                    "failed to query app status: {e}"
                )));
            }
        }
        operation
            .deletion_progress(
                &mut checkpoint,
                shared_types::UserAppDeletionStage::ComputeRemoved,
            )
            .await?;
        release_lock.mark_mutating()?;
        self.finish_captured_purge(app_id, operation, &mut checkpoint, dev_deletion)
            .await?;
        // Keep the file/runtime lease until the durable terminal record commits.
        Ok(())
    }

    /// Both purge entry points retain the captured targets through every stage.
    pub(crate) async fn finish_captured_purge(
        &self,
        app_id: &str,
        operation: &mut crate::service::OwnedOperation,
        checkpoint: &mut shared_types::UserAppDeletionCheckpoint,
        dev_deletion: Box<dyn shared_types::UserappDevDeletion>,
    ) -> AppResult<()> {
        if checkpoint.development.as_ref() != Some(&dev_deletion.receipt()) {
            return Err(AppOperationError::Conflict(
                "Development deletion ticket changed after capture".into(),
            ));
        }
        self.ensure_app_deleted(app_id, "destroying captured storage")
            .await?;
        operation.authorize_mutation().await?;
        self.runtime
            .destroy_app_storage_snapshot(&checkpoint.production)
            .await
            .map_err(|error| map_runtime_error("Destroy captured production storage", error))?;
        operation
            .deletion_progress(
                checkpoint,
                shared_types::UserAppDeletionStage::ProductionStorageRemoved,
            )
            .await?;
        operation.authorize_mutation().await?;
        dev_deletion.cleanup().await.map_err(|error| {
            AppOperationError::Backend(format!("destroy captured userapp dev resources: {error}"))
        })?;
        operation
            .deletion_progress(
                checkpoint,
                shared_types::UserAppDeletionStage::DevelopmentRemoved,
            )
            .await?;
        Ok(())
    }
}
