use super::*;

/// Keep captured local guards alive through the terminal SQL commit, including
/// failure commits. Moving execution to a helper must not shorten their lifetime.
#[derive(Default)]
pub(crate) struct StorageClearLeases {
    development: Option<Box<dyn shared_types::UserappDevDeletion>>,
    directories: Vec<shared_types::storage_contents::StorageDirectoryLease>,
}

impl crate::service::AppService {
    /// 清空应用持久存储内容（数据语义；卷对象去留按运行时能力与环境语义）。
    /// - `prod`：安全约束仅当 app 计算资源已不存在时允许（否则 INVALID_STATE）。
    ///   K8s/RBD：rcoder 不可挂载无法逐文件清 → 删 PVC（下次 create 自动重建空卷，
    ///   数据清空语义等价；handbook 注明）。Docker：清 prod 四目录内容（留目录本身）。
    /// - `dev`：经开发容器 file-server 清空 workspace 内容（**留容器留卷**——
    ///   "重置开发工作区"语义；开发容器常驻，卷重建要求先销毁容器，得不偿失）。
    ///   幂等；容器内为旧镜像（无 clear 端点）时 404 上抛 Backend。
    pub async fn clear_app_storage(&self, app_stage: UserappStage, app_id: &str) -> AppResult<()> {
        self.clear_app_storage_controlled(
            app_stage,
            app_id,
            ClearStorageRequest {
                lifecycle_id: None,
                request_id: None,
            },
        )
        .await
        .map(|_| ())
    }

    /// Execute only an already admitted clear under the caller's operation lease.
    /// Recovery observes an existing builder; it never ensures a builder while
    /// its own clear operation occupies the application's durable operation slot.
    pub(crate) async fn execute_storage_clear(
        &self,
        app_id: &str,
        production: bool,
        operation: &mut crate::service::OwnedOperation,
        guard: &crate::service::AppOperationGuard,
        leases: &mut StorageClearLeases,
    ) -> AppResult<()> {
        operation.bind_lease(guard).await?;
        let workspace_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|error| {
                AppOperationError::Backend(format!("Create direct workspace client: {error}"))
            })?;
        let target = if !production {
            let mut ticket = self.capture_dev_deletion(app_id).await?;
            let receipt = ticket.receipt();
            let context = operation.execution_context();
            let endpoint = ticket
                .workspace_endpoint(&context)
                .await
                .map_err(AppOperationError::Backend)?;
            let base_url = endpoint.base_url();
            let response = workspace_client
                .get(format!("{base_url}/api/v1/userapp/app-files/clear-target"))
                .query(&shared_types::UserAppWorkspaceClearProbe {
                    app_id: app_id.into(),
                })
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .map_err(|error| {
                    AppOperationError::Backend(format!("Observe workspace clear target: {error}"))
                })?;
            let response =
                crate::ops::files::check_status(response, "observe-clear-target", app_id).await?;
            let target: shared_types::UserAppWorkspaceClearTarget =
                response.json().await.map_err(|error| {
                    AppOperationError::Backend(format!("Decode workspace clear target: {error}"))
                })?;
            if target.app_id != app_id || target.instance_id.is_empty() {
                return Err(AppOperationError::Conflict(
                    "Workspace clear target identity mismatch".into(),
                ));
            }
            // The HTTP probe must be bracketed by uncached observations
            // of the same physical pod/container, under the builder lease.
            if ticket
                .workspace_endpoint(&context)
                .await
                .map_err(AppOperationError::Backend)?
                != endpoint
            {
                return Err(AppOperationError::Conflict(
                    "Builder endpoint changed during workspace target observation".into(),
                ));
            }
            leases.development = Some(ticket);
            shared_types::UserAppStorageClearTarget::Development {
                base_url,
                instance_id: target.instance_id,
                endpoint,
                receipt: Box::new(receipt),
            }
        } else {
            self.ensure_app_deleted(app_id, "clearing storage").await?;
            let snapshot = self
                .runtime
                .capture_app_deletion(app_id, None)
                .await
                .map_err(|error| map_runtime_error("Capture storage clear resources", error))?;
            if self.config.access_mode == crate::config::AppAccessMode::Docker {
                for path in self.app_prod_dirs(app_id).await? {
                    leases.directories.push(
                        shared_types::storage_contents::StorageDirectoryLease::capture(&path)
                            .await
                            .map_err(|error| {
                                map_io_error("Capture storage directory", error, false)
                            })?,
                    );
                }
            }
            let directories = leases
                .directories
                .iter()
                .map(|lease| lease.receipt.clone())
                .collect();
            shared_types::UserAppStorageClearTarget::Production {
                snapshot,
                directories,
            }
        };
        let evidence = shared_types::UserAppStorageClear {
            context: operation.execution_context(),
            target,
        };
        evidence.validate().map_err(AppOperationError::Conflict)?;
        let checkpoint = serde_json::to_value(&evidence).map_err(|error| {
            AppOperationError::Backend(format!("Encode storage clear target: {error}"))
        })?;
        operation
            .checkpoint("clear_target_captured", checkpoint.clone())
            .await?;
        operation.authorize_mutation().await?;
        match &evidence.target {
            shared_types::UserAppStorageClearTarget::Development {
                base_url,
                instance_id,
                ..
            } => {
                let ticket = leases.development.as_mut().ok_or_else(|| {
                    AppOperationError::Backend("Captured builder lease is unavailable".into())
                })?;
                ticket
                    .begin_external_mutation()
                    .map_err(AppOperationError::Backend)?;
                guard.mark_mutating()?;
                let response = workspace_client
                    .post(format!("{base_url}/api/v1/userapp/app-files/clear"))
                    .timeout(std::time::Duration::from_secs(120))
                    .json(&shared_types::UserAppWorkspaceClearRequest {
                        app_id: app_id.into(),
                        expected_instance_id: instance_id.clone(),
                    })
                    .send()
                    .await
                    .map_err(|error| {
                        AppOperationError::Backend(format!("Clear development workspace: {error}"))
                    })?;
                let response =
                    crate::ops::files::check_status(response, "clear-dev-workspace", app_id)
                        .await?;
                let result: shared_types::UserAppWorkspaceClearResult =
                    response.json().await.map_err(|error| {
                        AppOperationError::Backend(format!(
                            "Decode workspace clear result: {error}"
                        ))
                    })?;
                if !result.confirms(instance_id) {
                    return Err(AppOperationError::Backend(
                        "Workspace clear did not confirm success".into(),
                    ));
                }
                ticket
                    .finish_external_mutation()
                    .await
                    .map_err(AppOperationError::Backend)?;
            }
            shared_types::UserAppStorageClearTarget::Production {
                snapshot,
                directories,
            } => {
                self.ensure_app_deleted(app_id, "clearing captured storage")
                    .await?;
                if self.config.access_mode == crate::config::AppAccessMode::Docker {
                    if leases
                        .directories
                        .iter()
                        .map(|lease| &lease.receipt)
                        .ne(directories.iter())
                    {
                        return Err(AppOperationError::Conflict(
                            "Storage directory leases differ from captured evidence".into(),
                        ));
                    }
                    for directory in &leases.directories {
                        directory.validate_current().await.map_err(|error| {
                            map_io_error("Validate captured storage directory", error, false)
                        })?;
                    }
                }
                guard.mark_mutating()?;
                if self.config.access_mode == crate::config::AppAccessMode::Kubernetes {
                    self.runtime
                        .destroy_app_storage_snapshot(snapshot)
                        .await
                        .map_err(|error| map_runtime_error("Clear captured storage", error))?;
                } else {
                    for directory in &leases.directories {
                        directory.clear().await.map_err(|error| {
                            map_io_error("Clear captured storage directory", error, false)
                        })?;
                    }
                }
            }
        }
        operation
            .checkpoint("storage_contents_cleared", checkpoint)
            .await?;
        Ok::<(), AppOperationError>(())
    }

    pub async fn clear_app_storage_controlled(
        &self,
        app_stage: UserappStage,
        app_id: &str,
        request: ClearStorageRequest,
    ) -> AppResult<String> {
        use garde::Validate as _;
        use sha2::Digest as _;
        validate_app_id(app_id)?;
        request
            .validate()
            .map_err(|error| AppOperationError::Validation(error.to_string()))?;
        let guard = self
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
            let kind = match app_stage {
                UserappStage::Dev => shared_types::UserAppOperationKind::ClearDevStorage,
                UserappStage::Prod => shared_types::UserAppOperationKind::ClearProdStorage,
            };
            let fingerprint = hex::encode(sha2::Sha256::digest(
                shared_types::encode_userapp_intent(
                    &serde_json::json!({"stage":app_stage.as_str(), "request":request}),
                )
                .map_err(|error| {
                    AppOperationError::Backend(format!("Encode storage clear intent: {error}"))
                })?,
            ));
            let control = shared_types::UserAppControlRequest {
                lifecycle_id: request.lifecycle_id.clone(),
                request_id: request.request_id.clone(),
            };
            if let Some(existing) = self
                .replay_control(app_id, &control, kind, &fingerprint)
                .await?
            {
                return Ok(existing.operation_id);
            }
            // Ensure must finish before admitting clear: builder ensure has its
            // own durable operation and must not wait on the clear it is serving.
            if app_stage == UserappStage::Dev {
                self.app_files_base(app_stage, app_id).await?;
            }
            let operation_id = uuid::Uuid::new_v4().to_string();
            let mut operation = crate::service::OwnedOperation::admit(
                self.metadata.store.clone(),
                shared_types::UserAppAdmission {
                    runtime_policy_on_success: None,
                    metadata: None,
                    command: Some(shared_types::UserAppControlCommand::ClearStorage {
                        production: app_stage == UserappStage::Prod,
                    }),
                    kind,
                    app_id: app_id.into(),
                    lifecycle_id: request.lifecycle_id,
                    request_id: request.request_id,
                    operation_id: operation_id.clone(),
                    request_fingerprint: fingerprint,
                },
            )
            .await?;
            let mut clear_leases = StorageClearLeases::default();
            let mutation = self
                .execute_storage_clear(
                    app_id,
                    app_stage == UserappStage::Prod,
                    &mut operation,
                    &guard,
                    &mut clear_leases,
                )
                .await;
            match mutation {
                Ok(()) => {
                    operation.succeed().await?;
                    guard.mark_completed();
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
}
