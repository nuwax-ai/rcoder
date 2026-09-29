use super::*;

pub(super) async fn application_lease_root() -> Result<std::path::PathBuf> {
    #[cfg(feature = "deploy-host")]
    if shared_types::is_deploy_host() {
        if let Ok(root) = std::env::var("RCODER_OPERATION_LOCK_ROOT")
            && !root.trim().is_empty()
        {
            return Ok(std::path::PathBuf::from(root));
        }
        return crate::path::resolve_container_path_to_host(std::path::Path::new(
            shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT,
        ))
        .await
        .map_err(|error| {
            Error::DockerError(format!(
                "Resolve deploy-host application lease root: {error}"
            ))
        });
    }
    Ok(std::path::PathBuf::from(
        shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT,
    ))
}

impl DockerRuntime {
    pub(crate) async fn validate_captured_file_lease(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> Result<bool> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::ConfigurationError)?;
        receipt.validate().map_err(Error::ConfigurationError)?;
        let prefix = match receipt.service_type() {
            ServiceType::Userapp => "prod",
            ServiceType::UserappBuilder => "builder",
            _ => {
                return Err(Error::ConfigurationError(
                    "Invalid application lease family".into(),
                ));
            }
        };
        let path = application_lease_root()
            .await?
            .join(".app-operation-locks")
            .join(format!("{prefix}-{}.lock", context.app_id));
        let receipt = receipt.clone();
        tokio::task::spawn_blocking(move || validate_file_receipt(&path, &receipt))
            .await
            .map_err(|error| {
                Error::DockerError(format!(
                    "Operation lease verification worker failed: {error}"
                ))
            })?
    }
    pub(crate) async fn captured_file_lease_holder_dead(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> Result<bool> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::ConfigurationError)?;
        receipt.validate().map_err(Error::ConfigurationError)?;
        let prefix = match receipt.service_type() {
            ServiceType::Userapp => "prod",
            ServiceType::UserappBuilder => "builder",
            _ => {
                return Err(Error::ConfigurationError(
                    "Invalid application lease family".into(),
                ));
            }
        };
        let path = application_lease_root()
            .await?
            .join(".app-operation-locks")
            .join(format!("{prefix}-{}.lock", context.app_id));
        let receipt = receipt.clone();
        tokio::task::spawn_blocking(move || file_receipt_holder_dead(&path, &receipt))
            .await
            .map_err(|error| {
                Error::DockerError(format!(
                    "Operation lease liveness probe worker failed: {error}"
                ))
            })?
    }
    pub(crate) async fn captured_builder_workspace(
        &self,
        snapshot: &BuilderDeletionSnapshot,
        context: &shared_types::UserAppExecutionContext,
    ) -> Result<shared_types::UserAppBuilderWorkspaceEndpoint> {
        context
            .validate_identity(&snapshot.app_id)
            .map_err(Error::Conflict)?;
        let [resource] = snapshot.resources.as_slice() else {
            return Err(Error::Conflict(
                "Builder container receipt is ambiguous or missing".into(),
            ));
        };
        if resource.kind != AppResourceKind::Container || resource.uid.is_empty() {
            return Err(Error::Conflict(
                "Builder container receipt is invalid".into(),
            ));
        }
        let info = self
            .inner
            .get_docker_client()
            .inspect_container(&resource.uid, None)
            .await
            .map_err(|error| {
                Error::DockerError(format!("Inspect captured builder endpoint: {error}"))
            })?;
        workspace_endpoint_from_bound_container(
            &info,
            resource,
            context,
            snapshot.resource_binding.as_ref(),
        )
    }

    pub(crate) async fn acquire_builder_lease(
        &self,
        app_id: &str,
    ) -> Result<Box<dyn shared_types::AppOperationLease>> {
        self.acquire_application_file_lease(app_id, &ServiceType::UserappBuilder)
            .await
    }
    pub(crate) async fn acquire_application_file_lease(
        &self,
        app_id: &str,
        family: &ServiceType,
    ) -> Result<Box<dyn shared_types::AppOperationLease>> {
        self.acquire_application_file_lease_with_context(app_id, family, None)
            .await
    }

    pub(crate) async fn acquire_application_file_lease_with_context(
        &self,
        app_id: &str,
        family: &ServiceType,
        context: Option<&shared_types::UserAppExecutionContext>,
    ) -> Result<Box<dyn shared_types::AppOperationLease>> {
        let marker = if let Some(context) = context {
            context
                .validate_identity(app_id)
                .map_err(Error::ConfigurationError)?;
            shared_types::AppFileMutationMarker::for_operation(&context.operation_id)
                .map_err(|error| Error::ConfigurationError(error.to_string()))?
        } else {
            shared_types::AppFileMutationMarker::new()
        };
        let prefix = match family {
            ServiceType::Userapp => "prod",
            ServiceType::UserappBuilder => "builder",
            _ => {
                return Err(Error::ConfigurationError(
                    "application lease requires UserApp family".into(),
                ));
            }
        };
        if app_id.is_empty()
            || app_id.len() > 64
            || !app_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(Error::ConfigurationError(
                "invalid builder app identifier".into(),
            ));
        }
        let root = application_lease_root().await?.join(".app-operation-locks");
        let name = format!("{prefix}-{app_id}.lock");
        let mut pending = tokio::task::spawn_blocking(move || {
            lock_builder_file_with_marker(&root, &name, marker)
                .map(|lease| UnclaimedBuilderLease(Some(lease)))
        })
        .await
        .map_err(|e| Error::DockerError(format!("builder operation lease worker: {e}")))??;
        let mut lease = pending
            .0
            .take()
            .ok_or_else(|| Error::DockerError("builder lease was already claimed".into()))?;
        lease.service_type = *family;
        Ok(Box::new(lease))
    }
    pub(crate) async fn release_captured_file_lease(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> Result<()> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::ConfigurationError)?;
        receipt.validate().map_err(Error::ConfigurationError)?;
        // 穷尽列举：UserApp 租约域仅两形态合法，其余 ServiceType 显式拒绝
        // （fail-fast 收窄域——新增变体时编译期提醒本域是否要接）
        let prefix = match receipt.service_type() {
            ServiceType::Userapp => "prod",
            ServiceType::UserappBuilder => "builder",
            ServiceType::WebAgentRunner
            | ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject => {
                return Err(Error::ConfigurationError(
                    "Invalid application lease family".into(),
                ));
            }
        };
        let path = application_lease_root()
            .await?
            .join(".app-operation-locks")
            .join(format!("{prefix}-{}.lock", context.app_id));
        let receipt = receipt.clone();
        tokio::task::spawn_blocking(move || release_file_receipt(&path, &receipt))
            .await
            .map_err(|error| {
                Error::DockerError(format!("Operation lease cleanup worker failed: {error}"))
            })?
    }

    pub(crate) async fn capture_builder(&self, app_id: &str) -> Result<BuilderDeletionSnapshot> {
        let mut snapshot = BuilderDeletionSnapshot {
            resource_binding: None,
            app_id: app_id.into(),
            operation_id: uuid::Uuid::new_v4().to_string(),
            resources: vec![],
            docker_bind_cleanup: true,
        };
        // Read Docker directly: runtime find synthesizes service_type from the
        // request and may return cached IDs, neither proves physical ownership.
        let name = crate::utils::DockerUtils::generate_container_name(
            ServiceType::UserappBuilder.container_prefix(),
            app_id,
        )
        .map_err(Error::ConfigurationError)?;
        match self
            .inner
            .get_docker_client()
            .inspect_container(&name, None)
            .await
        {
            Ok(info) => snapshot
                .resources
                .push(builder_identity(info, &name, app_id)?),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {}
            Err(error) => {
                return Err(Error::DockerError(format!(
                    "capture builder container: {error}"
                )));
            }
        }
        Ok(snapshot)
    }

    pub(crate) async fn delete_captured_builder(
        &self,
        snapshot: &BuilderDeletionSnapshot,
    ) -> Result<()> {
        if !snapshot.docker_bind_cleanup {
            return Err(Error::ConfigurationError(
                "non-Docker builder receipt".into(),
            ));
        }
        let current = self.capture_builder(&snapshot.app_id).await?;
        if current
            .resources
            .iter()
            .any(|resource| !snapshot.resources.contains(resource))
        {
            return Err(Error::Conflict(
                "builder was replaced after deletion capture".into(),
            ));
        }
        for resource in &snapshot.resources {
            if resource.kind != AppResourceKind::Container || resource.uid.is_empty() {
                return Err(Error::ConfigurationError(
                    "invalid builder container receipt".into(),
                ));
            }
            match self
                .inner
                .get_docker_client()
                .remove_container(
                    &resource.uid,
                    Some(bollard::query_parameters::RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                Ok(()) => {}
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {}
                Err(error) => {
                    return Err(Error::DockerError(format!(
                        "delete captured builder: {error}"
                    )));
                }
            }
            self.inner
                .retire_container_cache(&resource.uid)
                .await
                .map_err(|error| {
                    Error::DockerError(format!("retire deleted container cache: {error}"))
                })?;
        }
        if !self
            .capture_builder(&snapshot.app_id)
            .await?
            .resources
            .is_empty()
        {
            return Err(Error::Conflict(
                "builder was replaced during deletion".into(),
            ));
        }
        Ok(())
    }
}
