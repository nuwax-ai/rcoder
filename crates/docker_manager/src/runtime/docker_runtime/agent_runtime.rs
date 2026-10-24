use super::*;

#[async_trait]
impl AgentContainerRuntime for DockerRuntime {
    fn agent_workload_namespace(&self) -> String {
        self.agent_retirement_namespace()
    }
    fn agent_workload_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        self.agent_retirement_name(identifier, service_type)
    }
    async fn agent_resource_inventory(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::AgentStopSnapshot>> {
        self.docker_agent_inventory().await
    }
    async fn capture_agent_stop(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentStopSnapshot> {
        self.capture_docker_agent_retirement(identifier, service_type)
            .await
    }
    async fn agent_compute_absent(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        registered: &ContainerBasicInfo,
    ) -> ContainerRuntimeResult<bool> {
        if self
            .capture_docker_agent_retirement(identifier, service_type)
            .await?
            .has_compute()
        {
            return Ok(false);
        }
        if registered.container_id.is_empty() {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Registered Agent has no physical container identity".into(),
            ));
        }
        // A renamed container may have lost the canonical name/labels but is
        // still execution. Only the original exact CID's 404 proves absence.
        match self
            .inner
            .get_docker_client()
            .inspect_container(&registered.container_id, None)
            .await
        {
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(true),
            Ok(_) => Ok(false),
            Err(error) => Err(ContainerRuntimeError::DockerError(format!(
                "Inspect captured Agent absence: {error}"
            ))),
        }
    }
    async fn stop_captured_agent(
        &self,
        snapshot: &container_runtime_api::AgentStopSnapshot,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentStopOutcome> {
        self.retire_docker_agent_snapshot(snapshot, false).await
    }
    async fn retire_agent_remnants(
        &self,
        snapshot: &container_runtime_api::AgentStopSnapshot,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentStopOutcome> {
        // Docker's remnant analog: once the engine has confirmed idleness,
        // references, and persistent-mount data protection, retirement is the
        // exact-CID removal. The shared executor re-verifies scope, UID, and
        // ownership labels, and treats a missing container as Complete.
        self.retire_docker_agent_snapshot(snapshot, true).await
    }
    async fn refresh_agent_retirement_fence(
        &self,
        snapshot: &container_runtime_api::AgentStopSnapshot,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentStopSnapshot> {
        // Docker has no resource-version fence. This is observation only and
        // never authorizes reopening a CID with an unknown removal in flight.
        let current = self
            .capture_docker_agent_retirement(&snapshot.identifier, &snapshot.service_type)
            .await?;
        if current.container.as_ref().map(|value| &value.uid)
            != snapshot.container.as_ref().map(|value| &value.uid)
        {
            return Err(ContainerRuntimeError::Conflict(
                "Docker Agent changed during admission".into(),
            ));
        }
        Ok(current)
    }
    async fn stop_agent_container_if_matches(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        expected_container_id: &str,
    ) -> ContainerRuntimeResult<()> {
        self.stop_captured_agent(identifier, service_type, expected_container_id)
            .await
    }

    async fn stop_idle_builder_compute(
        &self,
        identifier: &str,
        captured: &ContainerBasicInfo,
    ) -> ContainerRuntimeResult<()> {
        self.retire_captured_idle_builder(identifier, captured)
            .await
    }

    async fn get_bound_builder_file_credentials(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<shared_types::FileServerRequestCredentials> {
        self.read_builder_file_credentials(target).await
    }

    async fn start_builder_control_with_options(
        &self,
        target: &shared_types::BuilderControlTarget,
        options: &shared_types::BuilderStartOptions,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        if options.startup_mode != target.builder_startup_mode {
            return Err(ContainerRuntimeError::Conflict(
                "Builder startup intent differs from its checkpoint".into(),
            ));
        }
        self.start_builder_compute(target).await
    }

    async fn current_builder_image(&self) -> ContainerRuntimeResult<Option<String>> {
        self.inner
            .select_image(&ServiceType::UserappBuilder, None)
            .await
            .map(Some)
            .map_err(|error| {
                ContainerRuntimeError::ConfigurationError(format!(
                    "Select current builder image: {error}"
                ))
            })
    }

    async fn builder_image_replacement_needed(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<bool> {
        let Some(resource) = &target.workload else {
            return Ok(false);
        };
        let inspect = self
            .inner
            .get_docker_client()
            .inspect_container(&resource.uid, None)
            .await
            .map_err(|error| {
                ContainerRuntimeError::DockerError(format!("Inspect builder image: {error}"))
            })?;
        let management_boot_needed = target.builder_startup_mode
            == shared_types::BuilderStartupMode::ManagementOnly
            && !inspect
                .config
                .as_ref()
                .and_then(|config| config.env.as_ref())
                .is_some_and(|env| {
                    env.iter().any(|entry| {
                        entry == &format!("{}=true", shared_types::APP_CLI_CONTROL_ONLY_ENV)
                    }) && env.iter().any(|entry| {
                        entry
                            == &format!(
                                "{}={}",
                                shared_types::APP_CLI_MANAGEMENT_BOOT_ID_ENV,
                                target.context.operation_id
                            )
                    })
                });
        Ok(management_boot_needed
            || target.restart_image.as_ref().is_some_and(|image| {
                inspect
                    .config
                    .as_ref()
                    .and_then(|config| config.image.as_ref())
                    != Some(image)
            }))
    }

    async fn refresh_container_reach(
        &self,
        info: &ContainerBasicInfo,
    ) -> ContainerRuntimeResult<()> {
        #[cfg(feature = "deploy-host")]
        if shared_types::is_deploy_host() {
            let observation =
                shared_types::published::begin_physical_observation(&info.container_id);
            let preferred = std::env::var("RCODER_AGENT_NETWORK")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "rcoder-agent-network".into());
            let refresh = async {
                for attempt in 0..3 {
                    let inspect = self
                        .inner
                        .get_docker_client()
                        .inspect_container(&info.container_id, None)
                        .await
                        .map_err(|error| match error {
                            bollard::errors::Error::DockerResponseServerError {
                                status_code: 404,
                                ..
                            } => {
                                ContainerRuntimeError::ContainerNotFound(info.container_id.clone())
                            }
                            error => ContainerRuntimeError::DockerError(format!(
                                "Refresh container address: {error}"
                            )),
                        })?;
                    if inspect.id.as_deref() != Some(info.container_id.as_str())
                        || inspect
                            .name
                            .as_deref()
                            .map(|name| name.trim_start_matches('/'))
                            != Some(info.container_name.as_str())
                    {
                        return Err(ContainerRuntimeError::Conflict(
                            "Container address identity changed".into(),
                        ));
                    }
                    match crate::deploy_host_ports::register_reach_from_inspect(
                        &info.container_name,
                        Some(&preferred),
                        &inspect,
                        &observation,
                    ) {
                        Ok(()) => return Ok(()),
                        Err(error) if attempt == 2 => {
                            return Err(ContainerRuntimeError::ConnectionError(error.to_string()));
                        }
                        Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
                    }
                }
                Err(ContainerRuntimeError::ConnectionError(
                    "Container address is not ready".into(),
                ))
            };
            return tokio::time::timeout(Duration::from_secs(3), refresh)
                .await
                .map_err(|_| {
                    ContainerRuntimeError::ConnectionError(
                        "Container address refresh timed out".into(),
                    )
                })?;
        }
        let _ = info;
        Ok(())
    }

    async fn acquire_builder_operation(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Box<dyn shared_types::AppOperationLease>> {
        self.acquire_builder_lease(app_id).await
    }
    async fn capture_builder_deletion(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<shared_types::BuilderDeletionSnapshot> {
        self.capture_builder(app_id).await
    }
    async fn inspect_builder_deletion(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: Option<&shared_types::UserAppOperationLeaseReceipt>,
        snapshot: &shared_types::BuilderDeletionSnapshot,
    ) -> ContainerRuntimeResult<shared_types::DeletionInspection> {
        context
            .validate_identity(&snapshot.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        self.inspect_captured_docker_deletion(
            context,
            ServiceType::UserappBuilder,
            receipt,
            &snapshot.resources,
        )
        .await
    }
    async fn find_builder_instances(&self, app_id: &str) -> ContainerRuntimeResult<Vec<String>> {
        // Docker 形态：容器名 `rcoder-app-builder-{app_id}`（纯 app_id，无用户
        // 维度）——全量列举后按 builder 族 + identifier 过滤（含已停容器——清扫
        // 语义要覆盖非 Running 残留）。
        let mut result = Vec::new();
        for container in self.list_containers().await? {
            if container.service_type.as_ref() != Some(&ServiceType::UserappBuilder) {
                continue;
            }
            if let Some(identifier) = container.identity_key().filter(|id| *id == app_id) {
                result.push(identifier.to_string());
            }
        }
        Ok(result)
    }
    async fn delete_builder_snapshot(
        &self,
        snapshot: &shared_types::BuilderDeletionSnapshot,
    ) -> ContainerRuntimeResult<()> {
        self.delete_captured_builder(snapshot).await
    }
    async fn inspect_builder_workspace(
        &self,
        snapshot: &shared_types::BuilderDeletionSnapshot,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::UserAppBuilderWorkspaceEndpoint> {
        self.captured_builder_workspace(snapshot, context).await
    }
    async fn inspect_builder_candidate(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        self.capture_builder_compute_with_binding(context, None, true)
            .await
    }

    async fn capture_builder_adoption(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_container_id: &str,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        let target = self
            .capture_builder_compute_with_binding(context, None, true)
            .await?;
        if target
            .workload
            .as_ref()
            .map(|resource| resource.uid.as_str())
            != Some(expected_container_id)
            || expected_container_id.is_empty()
        {
            return Err(ContainerRuntimeError::Conflict(
                "Physical builder changed before adoption".into(),
            ));
        }
        Ok(target)
    }
    async fn capture_bound_builder_control(
        &self,
        context: &shared_types::UserAppExecutionContext,
        binding: Option<&shared_types::UserAppResourceBinding>,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        self.capture_builder_compute_with_binding(context, binding, false)
            .await
    }

    async fn recover_builder_creation(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<Option<shared_types::BuilderCreationEvidence>> {
        if let Some(receipt) =
            crate::runtime::docker_compute_receipt::read_cancellation(context).await?
        {
            self.release_captured_file_lease(context, &receipt.lease)
                .await?;
            return Err(ContainerRuntimeError::CreationCancelled);
        }
        let Some(receipt) = crate::runtime::docker_compute_receipt::read_creation(context).await?
        else {
            return Ok(None);
        };
        // Active local writers retain their flock; release checks original
        // inode/token and refuses a live writer or any replacement lock.
        self.release_captured_file_lease(context, &receipt.lease)
            .await?;
        Ok(Some(shared_types::BuilderCreationEvidence {
            registration_predecessor: None,
            creation_lease_released: true,
            target: receipt.target,
            container: receipt.container,
        }))
    }

    async fn observe_unbound_builder_creation_authority(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<container_runtime_api::UnboundBuilderCreationAuthority> {
        #[cfg(unix)]
        {
            self.observe_unbound_builder_creation_authority(context)
                .await
        }
        #[cfg(not(unix))]
        {
            let _ = context;
            Ok(container_runtime_api::UnboundBuilderCreationAuthority::Unsupported)
        }
    }

    async fn capture_builder_control(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        self.capture_builder_compute(context).await
    }

    async fn builder_compute_absent(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<bool> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let name = crate::utils::DockerUtils::generate_container_name(
            ServiceType::UserappBuilder.container_prefix(),
            &context.app_id,
        )
        .map_err(ContainerRuntimeError::ConfigurationError)?;
        let client = self.inner.get_docker_client();
        match client.inspect_container(&name, None).await {
            Ok(_) => return Ok(false),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {}
            Err(error) => {
                return Err(ContainerRuntimeError::DockerError(format!(
                    "Observe absent builder compute: {error}"
                )));
            }
        }
        // A renamed builder retains its physical execution and ownership
        // labels. Missing the canonical name does not prove it was removed.
        let filters = HashMap::from([(
            "label".to_string(),
            vec![
                format!(
                    "service-type={}",
                    ServiceType::UserappBuilder.container_family_key()
                ),
                format!("identifier={}", context.app_id),
            ],
        )]);
        let options = bollard::query_parameters::ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        let residual = client
            .list_containers(Some(options))
            .await
            .map_err(|error| {
                ContainerRuntimeError::DockerError(format!(
                    "List remaining builder compute: {error}"
                ))
            })?;
        if !residual.is_empty() {
            return Ok(false);
        }
        // Creation may have acquired the canonical name while the list was
        // being read. Observe it again before publishing absence.
        match client.inspect_container(&name, None).await {
            Ok(_) => Ok(false),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(true),
            Err(error) => Err(ContainerRuntimeError::DockerError(format!(
                "Recheck absent builder compute: {error}"
            ))),
        }
    }

    async fn exec_builder_control_target(
        &self,
        target: &shared_types::BuilderControlTarget,
        command: Vec<String>,
    ) -> ContainerRuntimeResult<container_runtime_api::ExecResult> {
        self.exec_bound_builder(target, command).await
    }

    async fn start_builder_control(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.start_builder_compute(target).await
    }

    async fn reconcile_builder_compute_stop(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<bool> {
        self.reconcile_builder_stop(target).await
    }

    async fn observe_builder_restart_stop(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<shared_types::BuilderRestartStopKind>> {
        self.observe_original_builder_restart_stop(target).await
    }

    async fn reconcile_builder_compute_start(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.reconcile_builder_start(target).await
    }

    async fn builder_compute_write_acknowledged(
        &self,
        target: &shared_types::BuilderControlTarget,
        starting: bool,
    ) -> ContainerRuntimeResult<bool> {
        if starting {
            crate::runtime::docker_compute_receipt::matches_start(target).await
        } else {
            crate::runtime::docker_compute_receipt::matches_stop(target).await
        }
    }

    async fn apply_builder_control(
        &self,
        target: &shared_types::BuilderControlTarget,
        restart: bool,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        let result = self.apply_builder_compute(target, restart).await?;
        if let Some(resource) = &target.workload {
            self.inner
                .retire_container_cache(&resource.uid)
                .await
                .map_err(|error| {
                    ContainerRuntimeError::DockerError(format!(
                        "Retire controlled builder cache: {error}"
                    ))
                })?;
        }
        Ok(result)
    }

    async fn observe_builder_compute_stopped(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<bool> {
        self.observe_retained_builder_stopped(target).await
    }

    async fn archive_builder_restart(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<shared_types::BuilderRestartTemplate>> {
        self.archive_builder_template(target).await
    }

    async fn recapture_stopped_builder_restart(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<shared_types::BuilderRestartTemplate> {
        self.recapture_stopped_builder_template(target).await
    }

    async fn inspect_builder_restart_archive(
        &self,
        template: &shared_types::BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<Option<bool>> {
        self.inspect_private_builder_archive(template).await
    }

    async fn restore_builder_restart(
        &self,
        template: &shared_types::BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        self.restore_builder_template(template).await
    }

    async fn capture_builder_compute_volumes(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<Vec<shared_types::AppResourceIdentity>> {
        self.capture_builder_volume_witness(target).await
    }

    async fn missing_builder_published_ports(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<Vec<u16>>> {
        #[cfg(feature = "deploy-host")]
        {
            if !shared_types::is_deploy_host() || crate::deploy_host_ports::is_direct_reach() {
                return Ok(None);
            }
            target.validate().map_err(ContainerRuntimeError::Conflict)?;
            let Some(resource) = target.workload.as_ref() else {
                return Ok(None);
            };
            let inspect = self
                .inner
                .get_docker_client()
                .inspect_container(&resource.uid, None)
                .await
                .map_err(|error| {
                    ContainerRuntimeError::DockerError(format!(
                        "Inspect published builder: {error}"
                    ))
                })?;
            if inspect.id.as_deref() != Some(resource.uid.as_str())
                || crate::runtime::docker_builder_control::control_identity_with_binding(
                    &inspect,
                    &resource.name,
                    &target.context,
                    target.resource_binding.as_ref(),
                    false,
                )? != *resource
            {
                return Err(ContainerRuntimeError::Conflict(
                    "Published builder identity changed".into(),
                ));
            }
            if inspect.state.as_ref().and_then(|state| state.running) != Some(true) {
                return Ok(None);
            }
            // A builder without its original workspace bind is not a safe
            // automatic repair candidate. The durable restart checks the exact
            // witness again before and after replacement.
            crate::runtime::docker_builder_restart::bind_witness(&inspect)?;
            return Ok(Some(crate::deploy_host_ports::missing_builder_ports(
                &inspect,
            )));
        }
        #[cfg(not(feature = "deploy-host"))]
        {
            let _ = target;
            Ok(None)
        }
    }

    async fn create_container(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        params.validate_execution_context()?;
        let captured = if params.service_type == ServiceType::UserappBuilder {
            self.capture_builder_reuse_snapshot(&params).await?
        } else {
            None
        };
        let prepared = if params.service_type == ServiceType::UserappBuilder
            && params.resource_binding.is_none()
            && captured
                .as_ref()
                .is_none_or(|view| view.target.workload.is_none())
        {
            Some(
                crate::agent_container_starter::AgentContainerStarter::new(&self.inner)
                    .preflight(&params)
                    .await
                    .map_err(|error| {
                        ContainerRuntimeError::ConfigurationError(error.to_string())
                    })?,
            )
        } else {
            None
        };
        let lease = if params.service_type == ServiceType::UserappBuilder {
            let identifier = params
                .service_type
                .container_identifier(
                    params.pod_id.as_deref(),
                    params.user_id.as_deref(),
                    params.project_id.as_deref(),
                )
                .map_err(|error| ContainerRuntimeError::ConfigurationError(error.to_string()))?;
            if let Some(context) = &params.execution_context {
                context
                    .validate_identity(identifier)
                    .map_err(ContainerRuntimeError::ConfigurationError)?;
            }
            Some(
                self.acquire_application_file_lease_with_context(
                    identifier,
                    &params.service_type,
                    params.execution_context.as_ref(),
                )
                .await?,
            )
        } else {
            None
        };
        if let Some(lease) = lease {
            let inner = self.inner.clone();
            let context = params.execution_context.clone();
            let binding = params.resource_binding.clone();
            return tokio::spawn(async move {
                let observed = match Self::new(inner.clone()).capture_builder_reuse_snapshot(&params).await {
                    Ok(observed) => observed,
                    Err(error) => return crate::runtime::builder_completion::finish(lease, Err(error)).await,
                };
                if let Some(original) = captured.as_ref().filter(|view| view.target.workload.is_some())
                    && observed.as_ref().is_none_or(|fresh| fresh.target.workload != original.target.workload
                        || fresh.volumes != original.volumes)
                {
                    return crate::runtime::builder_completion::finish(lease,
                        Err(ContainerRuntimeError::RequestRejected(shared_types::RuntimeRequestRejection { status: 409, message: "Existing builder identity or workspace changed while acquiring its lease".into() }))).await;
                }
                let captured = observed;
                let result = if params.resource_binding.is_some()
                    || captured.as_ref().is_some_and(|view| view.target.workload.is_some()) {
                    let Some(view) = captured.as_ref() else {
                        return crate::runtime::builder_completion::finish(lease,
                            Err(ContainerRuntimeError::ConfigurationError("builder reuse capture missing".into()))).await;
                    };
                    Self::new(inner.clone())
                        .resume_captured_builder(&params, view.clone(), None)
                        .await
                } else {
                    let Some(prepared) = prepared else {
                        return crate::runtime::builder_completion::finish(lease,
                            Err(ContainerRuntimeError::ConfigurationError("builder preflight result missing".into()))).await;
                    };
                    let result = crate::agent_container_starter::AgentContainerStarter::new(&inner)
                        .start_prepared(params.clone(), prepared)
                        .await;
                    let partial_id = result.as_ref().err()
                        .and_then(acknowledged_partial_creation)
                        .map(str::to_owned);
                    let result = result.map_err(crate::runtime::builder_completion::docker_error);
                    if let (Some(context), Some(physical_id)) = (context.as_ref(), partial_id.as_deref())
                    {
                        // Save known-complete writes before returning the original
                        // failure. Recovery can drain this exact created resource;
                        // it must still separately observe management readiness.
                        let runtime = Self::new(inner.clone());
                        crate::runtime::builder_completion::record_partial_creation(
                            result,
                            runtime.record_partial_creation(context, physical_id, lease.receipt()),
                        ).await
                    } else {
                        result
                    }
                };
                let result = match (result, context.as_ref()) {
                    (Ok(info), Some(context)) => {
                        let recorded = async {
                            crate::native_domain::reconcile_bounded(
                                inner.get_docker_client(),
                                &info.container_id,
                                &context.app_id,
                            )
                            .await;
                            let mut target = Self::new(inner.clone())
                                .capture_builder_compute_with_binding(
                                    context,
                                    binding.as_ref(),
                                    false,
                                )
                                .await?;
                            target.builder_startup_mode = params.builder_startup_mode;
                            if captured.as_ref().is_some_and(|view| view.target.workload.is_some()) {
                                let view = Self::new(inner.clone()).capture_builder_reuse_snapshot(&params).await
                                    .map_err(|error| ContainerRuntimeError::ConnectionError(format!("Confirm builder after reuse: {error}")))?
                                    .ok_or_else(|| ContainerRuntimeError::Conflict("Builder capture missing before completion".into()))?;
                                let original = captured.as_ref().ok_or_else(|| ContainerRuntimeError::Conflict("Builder original capture missing".into()))?;
                                if view.target.workload != original.target.workload || view.volumes != original.volumes {
                                    return Err(ContainerRuntimeError::Conflict("Builder identity or workspace changed before completion".into()));
                                }
                            }
                            let receipt = crate::runtime::builder_creation_receipt::BuilderCreationReceipt {
                                target,
                                container: info.clone(),
                                lease: lease.receipt().ok_or_else(|| {
                                    ContainerRuntimeError::ConfigurationError(
                                        "Builder creation lease receipt missing".into(),
                                    )
                                })?,
                            };
                            crate::runtime::docker_compute_receipt::save_creation(&receipt).await
                        }
                        .await;
                        recorded.map(|()| info)
                    }
                    (Err(ContainerRuntimeError::CreationCancelled), Some(context)) => {
                        let recorded = async {
                            let receipt =
                                crate::runtime::builder_creation_receipt::BuilderCancellationReceipt {
                                    context: context.clone(),
                                    lease: lease.receipt().ok_or_else(|| {
                                        ContainerRuntimeError::ConfigurationError(
                                            "Builder cancellation lease receipt missing".into(),
                                        )
                                    })?,
                                };
                            crate::runtime::docker_compute_receipt::save_cancellation(&receipt).await
                        }
                        .await;
                        match recorded {
                            Ok(()) => Err(ContainerRuntimeError::CreationCancelled),
                            Err(error) => Err(error),
                        }
                    }
                    (result, _) => result,
                };
                crate::runtime::builder_completion::finish(lease, result).await
            })
            .await
            .map_err(|e| {
                ContainerRuntimeError::ContainerCreationError(format!(
                    "builder creation worker: {e}"
                ))
            })?;
        }
        #[allow(deprecated)]
        self.inner
            .start_agent_container(params)
            .await
            // 集中转换保留提交阶段（发写前失败/明确拒绝/传输未知/已确认 CID），
            // 不再整体压平成 ContainerCreationError
            .map_err(crate::runtime::builder_completion::docker_error)
    }

    async fn get_container_info(
        &self,
        project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.inner
            .get_agent_info(project_id)
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))
    }

    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        match service_type {
            ServiceType::WebAgentRunner => self
                .inner
                .get_agent_info(identifier)
                .await
                .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string())),
            // 使用 find_container 实时查询 Docker API 获取 IP，
            // 避免 get_user_container_info → get_agent_info → get_container_info 只查缓存
            // 导致服务重启后缓存丢失返回 None。
            // UserappBuilder 复用 agent-runner 镜像(有 gRPC),同样走实时查询。
            // ComputerNormalProject（常规项目）与 Computer 共享容器，同走实时查询。
            ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject
            | ServiceType::UserappBuilder => {
                let result = self.find_container(identifier, service_type).await?;
                Ok(result.map(|pod| ContainerBasicInfo {
                    container_id: pod.container_id,
                    container_name: pod.container_name,
                    container_ip: pod.container_ip.clone(),
                    internal_port: shared_types::GRPC_DEFAULT_PORT,
                    external_port: 0,
                    project_id: identifier.to_string(),
                    status: String::from(pod.status),
                    created_at: pod.created_at,
                    service_url: format!(
                        "http://{}:{}",
                        pod.container_ip,
                        shared_types::GRPC_DEFAULT_PORT
                    ),
                    workload_uid: None,
                }))
            }
            // Userapp 兜底：Userapp 通常走 create_deployment/get_deployment_status，
            // 此处仅为 trait 穷尽性，端口不固定故 internal_port=0
            ServiceType::Userapp => {
                let result = self.find_container(identifier, service_type).await?;
                Ok(result.map(|pod| ContainerBasicInfo {
                    container_id: pod.container_id,
                    container_name: pod.container_name,
                    container_ip: pod.container_ip.clone(),
                    internal_port: 0,
                    external_port: 0,
                    project_id: identifier.to_string(),
                    status: String::from(pod.status),
                    created_at: pod.created_at,
                    // 唯一消费方（DockerRuntimeIpResolver）只读 container_ip；
                    // service_url 是 v1 agent 容器遗留字段，此处填空避免每请求死分配
                    service_url: String::new(),
                    workload_uid: None,
                }))
            }
        }
    }

    async fn find_container(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        // UserAppBuilder discovery feeds lifecycle verification and lazy ensure.
        // An actor/Moka snapshot can still say Running after its physical container
        // disappeared. Read Docker here; ownership remains checked by the existing
        // bound-capture and lifecycle gates before any routing or compute write.
        let result = if *service_type == ServiceType::UserappBuilder {
            let name = crate::utils::DockerUtils::generate_container_name(
                service_type.container_prefix(),
                project_id,
            )
            .map_err(ContainerRuntimeError::ConfigurationError)?;
            self.inner.find_container_authoritative(&name).await
        } else {
            self.inner
                .find_project_container(project_id, service_type)
                .await
        }
        .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))?;

        Ok(result.map(|r| {
            // Docker find 按类型化精确名查询——入参即创建时的派生主键
            // （ContainerConfigBuilder 的 project_id 槽存的就是 container_identifier
            // 派生值），按类型还原语义槽位
            let slots = container_runtime_api::slots_from_identifier(service_type, project_id);
            RuntimeContainerInfo {
                container_id: r.container_id,
                container_name: r.container_name,
                container_ip: r.container_ip,
                status: map_container_status(&r.status),
                created_at: r.created_at,
                env_vars: None, // 不填充环境变量（用于快速查找）
                service_type: Some(*service_type),
                project_id: slots.project_id,
                user_id: slots.user_id,
                pod_id: slots.pod_id,
                app_id: slots.app_id,
                workload_uid: None,
            }
        }))
    }

    /// Docker 基础诊断：find_container → docker inspect → 读 State(OOMKilled/exit_code/running)。
    /// 用于 gRPC 连接失败时的根因识别（OOM / 容器不在），生成精准友好错误，而非裸 transport error。
    /// inspect 失败(容器重建中等)→ 返回默认诊断(无根因)，调用方按"保留原文"处理，不误判。
    async fn diagnose_agent_pod(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentPodDiagnostic> {
        use bollard::query_parameters::InspectContainerOptions;

        // 1. 按 identifier 找容器；找不到本身就是根因（exists=false）。
        let Some(info) = self.find_container(identifier, service_type).await? else {
            return Ok(container_runtime_api::AgentPodDiagnostic {
                exists: false,
                ..Default::default()
            });
        };

        // 2. docker inspect 读 State。
        let inspect = match self
            .inner
            .get_docker_client()
            .inspect_container(&info.container_id, None::<InspectContainerOptions>)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "[DIAGNOSE] docker inspect failed for {}: {}",
                    info.container_id,
                    e
                );
                // inspect 失败(容器可能在重建/重命名)→ 不确定，返回默认诊断，不误判。
                return Ok(container_runtime_api::AgentPodDiagnostic::default());
            }
        };

        // 3. State → AgentPodDiagnostic。OOMKilled 是 Docker 侧最关键的根因信号。
        // state 借用(不 move inspect.state),以便同时读 inspect.restart_count(在顶层)。
        let state = inspect.state.as_ref();
        let oom_killed = state.and_then(|s| s.oom_killed).unwrap_or(false);
        let exit_code = state.and_then(|s| s.exit_code);
        let running = state.and_then(|s| s.running).unwrap_or(false);
        let restart_count = inspect.restart_count.unwrap_or(0) as u32;
        let status_str = state.and_then(|s| s.status.as_ref().map(|st| format!("{st:?}")));

        Ok(container_runtime_api::AgentPodDiagnostic {
            evidence: docker_diagnostic_evidence(&inspect),
            exists: true,
            ready: running,
            restart_count,
            last_terminate_reason: oom_killed.then(|| "OOMKilled".to_string()),
            last_exit_code: exit_code.map(|c| c as i32),
            waiting_reason: None, // Docker 无 CrashLoopBackOff 概念
            detail: status_str.filter(|s| !s.is_empty()),
        })
    }

    async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()> {
        self.inner
            .stop_container(project_id)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))
    }

    async fn stop_container_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        match service_type {
            ServiceType::UserappBuilder => {
                if let Some(container) = self.find_container(identifier, service_type).await? {
                    let result = tokio::time::timeout(
                        Duration::from_secs(8),
                        crate::runtime::docker_app_runtime::execute_container_command(
                            self.inner.get_docker_client(),
                            &container.container_id,
                            vec![
                                "app-cli".into(),
                                "--app-cli-container-stop".into(),
                                format!("/home/user/{identifier}"),
                            ],
                        ),
                    )
                    .await;
                    if !matches!(&result, Ok(Ok(exit)) if exit.exit_code == 0) {
                        tracing::warn!(%identifier, ?result, "Owner drain incomplete; proceeding with physical idle stop");
                    }
                    self.inner
                        .stop_container_by_id(&container.container_id)
                        .await
                        .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))?;
                }
                Ok(())
            }
            // Userapp/UserappBuilder 的 identifier=app_id/project_id，复用 WebAgentRunner 的 stop_container 路径
            ServiceType::WebAgentRunner | ServiceType::Userapp => self
                .inner
                .stop_container(identifier)
                .await
                .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string())),
            ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject => {
                if let Some(container) = self
                    .inner
                    .find_user_container(identifier, service_type)
                    .await
                    .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))?
                {
                    self.inner
                        .stop_container_by_id(&container.container_id)
                        .await
                        .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))?;
                }
                Ok(())
            }
        }
    }

    async fn is_container_running(&self, project_id: &str) -> ContainerRuntimeResult<bool> {
        if let Some(info) = self.get_container_info(project_id).await? {
            // 统一走 ContainerStatus 枚举比较（大小写不敏感），不直接比字符串
            Ok(crate::types::ContainerStatus::from(info.status).is_running())
        } else {
            Ok(false)
        }
    }

    async fn is_container_running_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self
            .find_container(identifier, service_type)
            .await?
            .map(|c| c.status == ContainerRuntimeStatus::Running)
            .unwrap_or(false))
    }

    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        let epoch = self.inner.containers.list_epoch();
        // 命中且代次一致才可用；状态已变更（创建/删除/更新）则强制刷新
        if let Some((cached_epoch, cached)) = self.list_cache.get(&()).await
            && cached_epoch == epoch
        {
            return Ok(cached);
        }

        let result = self.fetch_containers().await?;
        // 存 fetch 前代次：fetch 期间发生的变更会让下次读取失效重取，
        // 不会把变更前的快照误标为最新
        self.list_cache.insert((), (epoch, result.clone())).await;
        Ok(result)
    }

    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        self.inner
            .sync_all_container_states()
            .await
            .map_err(|e| ContainerRuntimeError::DockerError(e.to_string()))
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        self.inner
            .cleanup_all_containers()
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))
    }

    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        self.inner.get_docker_client().ping().await.map_err(|e| {
            ContainerRuntimeError::ConnectionError(format!("Docker ping failed: {}", e))
        })?;
        Ok(())
    }
}

fn docker_diagnostic_evidence(
    inspect: &bollard::models::ContainerInspectResponse,
) -> Option<container_runtime_api::AgentPodDiagnosticEvidence> {
    let physical_id = inspect.id.clone().filter(|id| !id.trim().is_empty())?;
    let state = inspect.state.as_ref();
    Some(container_runtime_api::AgentPodDiagnosticEvidence {
        physical_id,
        termination_finished_at: state
            .and_then(|s| s.finished_at.as_deref())
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&chrono::Utc)),
        current_termination: !state.and_then(|s| s.running).unwrap_or(false),
    })
}

#[cfg(test)]
#[path = "agent_runtime_tests.rs"]
mod tests;
