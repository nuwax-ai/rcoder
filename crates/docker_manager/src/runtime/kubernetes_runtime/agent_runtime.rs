use super::*;
use crate::runtime::k8s_pod::K8sPodOps;

#[cfg(feature = "kubernetes")]
#[async_trait]
impl AgentContainerRuntime for KubernetesRuntime {
    fn agent_workload_namespace(&self) -> String {
        self.namespace.clone()
    }
    fn agent_workload_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        self.pod_name(identifier, service_type)
    }
    async fn agent_resource_inventory(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::AgentStopSnapshot>> {
        self.agent_retirement_inventory().await
    }
    async fn capture_agent_stop(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentStopSnapshot> {
        self.capture_agent_retirement(identifier, service_type)
            .await
    }
    async fn agent_compute_absent(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        registered: &ContainerBasicInfo,
    ) -> ContainerRuntimeResult<bool> {
        self.observe_agent_compute_absence(identifier, service_type, registered)
            .await
    }
    async fn stop_captured_agent(
        &self,
        snapshot: &container_runtime_api::AgentStopSnapshot,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentStopOutcome> {
        self.retire_agent_snapshot(snapshot, false).await
    }
    async fn retire_agent_remnants(
        &self,
        snapshot: &container_runtime_api::AgentStopSnapshot,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentStopOutcome> {
        self.retire_agent_snapshot(snapshot, true).await
    }
    async fn refresh_agent_retirement_fence(
        &self,
        snapshot: &container_runtime_api::AgentStopSnapshot,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentStopSnapshot> {
        self.fence_agent_snapshot(snapshot).await
    }
    async fn agent_resource_events(
        &self,
        shutdown: tokio_util::sync::CancellationToken,
        interval: std::time::Duration,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentResourceEvents> {
        Ok(self.watch_agent_resources(shutdown, interval))
    }
    async fn stop_agent_container_if_matches(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        expected_container_id: &str,
    ) -> ContainerRuntimeResult<()> {
        self.stop_captured_agent_workload(identifier, service_type, expected_container_id)
            .await
    }

    async fn verify_agent_replacement(
        &self,
        params: &ContainerCreateParams,
        previous: Option<&ContainerBasicInfo>,
        replacement: &ContainerBasicInfo,
    ) -> ContainerRuntimeResult<container_runtime_api::VerifiedAgentReplacement> {
        self.verify_agent_registration_replacement(params, previous, replacement)
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
        if let Some(template) = &options.restart_template {
            self.start_builder_with_restart_template(target, template)
                .await
        } else {
            self.start_builder_compute(target).await
        }
    }

    async fn acquire_builder_operation(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Box<dyn shared_types::AppOperationLease>> {
        self.acquire_application_operation(app_id, &ServiceType::UserappBuilder)
            .await
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
        self.inspect_captured_deletion(
            context,
            ServiceType::UserappBuilder,
            receipt,
            &snapshot.resources,
        )
        .await
    }
    async fn find_builder_instances(&self, app_id: &str) -> ContainerRuntimeResult<Vec<String>> {
        self.find_builder_instances(app_id).await
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
        if target.pod.as_ref().map(|pod| pod.uid.as_str()) != Some(expected_container_id)
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

    async fn verify_absent_builder_recovery(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<bool> {
        self.verify_absent_retained_builder(context).await
    }

    async fn recover_builder_creation(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<Option<shared_types::BuilderCreationEvidence>> {
        if let Some(receipt) = self.read_builder_cancellation(context).await? {
            self.release_captured_application_operation(context, &receipt.lease)
                .await?;
            return Err(ContainerRuntimeError::CreationCancelled);
        }
        let receipt = match self.read_builder_creation_receipt(context).await? {
            Some(receipt) => Some(receipt),
            None => self.read_builder_service_creation(context).await?,
        };
        let Some(receipt) = receipt else {
            return Ok(None);
        };
        let container = receipt.registration_container()?;
        self.release_captured_application_operation(context, &receipt.lease)
            .await?;
        Ok(Some(shared_types::BuilderCreationEvidence {
            registration_predecessor: None,
            creation_lease_released: true,
            target: receipt.target,
            container,
        }))
    }

    async fn observe_unbound_builder_creation_authority(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<container_runtime_api::UnboundBuilderCreationAuthority> {
        self.observe_unbound_builder_creation_authority(context)
            .await
    }

    async fn capture_builder_orphan_stop(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<Option<shared_types::BuilderOrphanStopTarget>> {
        self.capture_orphan_stop(context, None, false).await
    }
    async fn inspect_builder_orphan_stop(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<Option<shared_types::BuilderOrphanStopTarget>> {
        self.capture_orphan_stop(context, None, true).await
    }
    async fn capture_bound_builder_orphan_stop(
        &self,
        context: &shared_types::UserAppExecutionContext,
        binding: Option<&shared_types::UserAppResourceBinding>,
    ) -> ContainerRuntimeResult<Option<shared_types::BuilderOrphanStopTarget>> {
        self.capture_orphan_stop(context, binding, false).await
    }
    async fn stop_builder_orphan(
        &self,
        target: &shared_types::BuilderOrphanStopTarget,
    ) -> ContainerRuntimeResult<()> {
        self.apply_orphan_stop(target).await
    }
    async fn confirm_builder_orphan_stopped(
        &self,
        target: &shared_types::BuilderOrphanStopTarget,
    ) -> ContainerRuntimeResult<bool> {
        self.observe_orphan_stopped(target).await
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
        let name = self.pod_name(&context.app_id, &ServiceType::UserappBuilder)?;
        if self
            .statefulsets()
            .get_opt(&name)
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!(
                    "Observe absent builder controller: {error}"
                ))
            })?
            .is_some()
        {
            return Ok(false);
        }
        self.confirm_builder_compute_absent(context).await?;
        Ok(true)
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

    async fn ensure_absent_builder_control(
        &self,
        target: &shared_types::BuilderControlTarget,
        volumes: &[shared_types::AppResourceIdentity],
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.ensure_absent_builder_compute(target, volumes, receipt)
            .await
    }

    async fn builder_compute_write_acknowledged(
        &self,
        target: &shared_types::BuilderControlTarget,
        starting: bool,
    ) -> ContainerRuntimeResult<bool> {
        if starting && target.workload.is_none() {
            return self.absent_builder_creation_acknowledged(target).await;
        }
        Ok(false)
    }

    async fn observe_completed_builder_creation(
        &self,
        target: &shared_types::BuilderControlTarget,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.observe_original_builder_creation(target, receipt)
            .await
    }

    async fn start_builder_control_with_restart(
        &self,
        target: &shared_types::BuilderControlTarget,
        template: &shared_types::BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.start_builder_with_restart_template(target, template)
            .await
    }

    async fn current_builder_image(&self) -> ContainerRuntimeResult<Option<String>> {
        Ok(Some(
            self.select_image(&ServiceType::UserappBuilder)
                .trim()
                .to_string(),
        ))
    }

    async fn apply_builder_control(
        &self,
        target: &shared_types::BuilderControlTarget,
        restart: bool,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        let result = self.apply_builder_compute(target, restart).await?;
        if let Some(pod) = &target.pod {
            let mut cache = self.pod_cache.write().await;
            if cache.get(&target.context.app_id).is_some_and(|cached| {
                cached.service_type == ServiceType::UserappBuilder
                    && cached.info.container_id == pod.uid
            }) {
                cache.remove(&target.context.app_id);
            }
        }
        Ok(result)
    }

    async fn reconcile_builder_compute_stop(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<bool> {
        self.reconcile_builder_stop_receipt(target).await
    }

    async fn observe_builder_compute_stopped(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<bool> {
        self.observe_retained_builder_stopped(target).await
    }

    fn supports_builder_compute_fencing(&self) -> bool {
        true
    }

    async fn archive_builder_restart(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<shared_types::BuilderRestartTemplate>> {
        self.archive_builder_template(target).await.map(Some)
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

    async fn fence_builder_compute_write(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<bool> {
        self.fence_builder_conditional_write(target).await
    }

    async fn reconcile_builder_compute_start(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.reconcile_builder_start_receipt(target).await
    }

    async fn prepare_builder_compute_retry(
        &self,
        target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<shared_types::BuilderControlTarget>> {
        self.prepare_builder_conditional_retry(target).await
    }

    async fn create_container(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        params.validate_execution_context()?;
        let captured = if params.service_type == ServiceType::UserappBuilder {
            match self.capture_builder_reuse_snapshot(&params).await {
                Ok(captured) => captured,
                Err(error)
                    if params.retained_storage.is_some()
                        && matches!(error, ContainerRuntimeError::Conflict(_)) =>
                {
                    None
                }
                Err(error) => {
                    return Err(ContainerRuntimeError::CreationAborted {
                        progress: container_runtime_api::CreationProgress {
                            failed_at: container_runtime_api::CreationStage::Capture,
                            definitive_rejection: error.is_definitive_rejection(),
                            retained_idempotent_resources: Vec::new(),
                        },
                        source: Box::new(error),
                    });
                }
            }
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
                self.acquire_application_operation_with_context(
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
            let runtime = Self {
                client: self.client.clone(),
                namespace: self.namespace.clone(),
                config: self.config.clone(),
                pod_cache: self.pod_cache.clone(),
                subvolume_path_cache: self.subvolume_path_cache.clone(),
                event_publisher: self.event_publisher.clone(),
                event_counters: self.event_counters.clone(),
            };
            let context = params.execution_context.clone();
            let binding = params.resource_binding.clone();
            return tokio::spawn(async move {
                if let Some(storage) = &params.retained_storage {
                    let context = params.execution_context.as_ref().ok_or_else(|| ContainerRuntimeError::ConfigurationError("Storage recovery operation missing".into()))?;
                    if let Err(error) = runtime.recover_retained_storage(context, storage).await {
                        let progress = container_runtime_api::CreationProgress {
                            failed_at: container_runtime_api::CreationStage::StorageClaim,
                            definitive_rejection: error.is_definitive_rejection(),
                            retained_idempotent_resources: storage.volumes.iter().map(|v|format!("retained PVC {} uid={}",v.name,v.uid)).collect(),
                        };
                        return crate::runtime::builder_completion::finish(lease, Err(ContainerRuntimeError::CreationAborted {progress,source:Box::new(error)})).await;
                    }
                }
                let observed = match runtime.capture_builder_reuse_snapshot(&params).await {
                    Ok(observed) => observed,
                    Err(error) => return crate::runtime::builder_completion::finish(lease, Err(error)).await,
                };
                if let Some(original) = captured.as_ref().filter(|view| view.target.workload.is_some())
                    && observed.as_ref().is_none_or(|fresh| {
                        fresh.target.workload.as_ref().map(|value| (&value.uid, &value.name))
                            != original.target.workload.as_ref().map(|value| (&value.uid, &value.name))
                            || fresh.volumes != original.volumes
                    })
                {
                    return crate::runtime::builder_completion::finish(lease,
                        Err(ContainerRuntimeError::RequestRejected(shared_types::RuntimeRequestRejection { status: 409, message: "Existing builder identity or workspace changed while acquiring its lease".into() }))).await;
                }
                let captured = observed;
                let was_existing = params.resource_binding.is_some()
                    || captured.as_ref().is_some_and(|view| view.target.workload.is_some());
                let result = if was_existing {
                    let Some(captured) = captured.as_ref() else {
                        return crate::runtime::builder_completion::finish(lease,
                            Err(ContainerRuntimeError::ConfigurationError("builder reuse capture missing".into()))).await;
                    };
                    // Reuse bypasses create_agent_container_with_creation_receipt.
                    // Fence every retained stable resource under this original
                    // builder lease before an old orphan DELETE can reach it.
                    let fencing = async {
                        let previous = runtime.capture_agent_retirement(
                            &captured.target.context.app_id, &ServiceType::UserappBuilder,
                        ).await?;
                        runtime.fence_agent_snapshot(&previous).await?;
                        Ok::<_, ContainerRuntimeError>(())
                    }.await;
                    if let Err(error) = fencing {
                        return crate::runtime::builder_completion::finish(lease, Err(error)).await;
                    }
                    runtime
                        .resume_captured_builder(&params, captured.clone())
                        .await
                        // RV07：本操作内执行了受控物理替换时，最终捕获与
                        // 收据改用新物理绑定——旧 UID 绑定对替换后的 STS
                        // 必然失配，不能让升级成功后收据失败。
                        .map(|(info, upgraded)| (info, upgraded.or(binding.clone())))
                } else {
                    runtime
                        .create_agent_container_with_creation_receipt(params.clone(), lease.receipt())
                        .await
                        .map(|info| (info, binding.clone()))
                };
                let result = match (result, context.as_ref()) {
                    (Ok((info, effective_binding)), Some(context)) => {
                        let recorded = async {
                            let mut target = runtime
                                .capture_builder_compute_with_binding(
                                    context,
                                    effective_binding.as_ref(),
                                    false,
                                )
                                .await?;
                            target.builder_startup_mode = params.builder_startup_mode;
                            if was_existing {
                                let fresh = runtime.capture_builder_reuse_snapshot(&params).await
                                    .map_err(|error| ContainerRuntimeError::ConnectionError(format!("Confirm builder after reuse: {error}")))?
                                    .ok_or_else(|| ContainerRuntimeError::Conflict("Builder capture missing before completion".into()))?;
                                let original = captured.as_ref().ok_or_else(|| ContainerRuntimeError::Conflict("Builder original capture missing".into()))?;
                                if fresh.target.workload.as_ref().map(|value| (&value.uid, &value.name))
                                    != original.target.workload.as_ref().map(|value| (&value.uid, &value.name))
                                    || fresh.volumes != original.volumes
                                {
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
                            if was_existing {
                                // A resumed builder needs the same final Service
                                // commit as a newly created one. Keep its proof
                                // in that mutation so a crash before the archive
                                // write can still be recovered without replay.
                                runtime.commit_builder_service_creation(&receipt).await?;
                            }
                            runtime.save_builder_creation_receipt(&receipt).await
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
                            runtime.save_builder_cancellation(&receipt).await
                        }
                        .await;
                        match recorded {
                            Ok(()) => Err(ContainerRuntimeError::CreationCancelled),
                            Err(error) => Err(error),
                        }
                    }
                    (result, _) => result.map(|(info, _)| info),
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
        self.create_agent_container(params).await
    }

    async fn get_container_info(
        &self,
        identifier: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        // trait 契约本就是 agent 族管理面（WebAgentRunner 语义，见
        // AgentContainerRuntime 文档注释）；显式类型化后 label 查询带 service-type
        // 维度，不再以 instance 单键捞到生产 UserApp pod。
        self.get_container_info_inner(identifier, &ServiceType::WebAgentRunner)
            .await
    }

    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.get_container_info_by_identifier_inner(identifier, service_type)
            .await
    }

    async fn find_container(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        self.find_container_inner(identifier, service_type).await
    }

    async fn find_container_for_ensure(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        if matches!(
            service_type,
            ServiceType::WebAgentRunner
                | ServiceType::ComputerAgentRunner
                | ServiceType::ComputerNormalProject
        ) {
            self.find_container_uncached(identifier, service_type).await
        } else {
            self.find_container_inner(identifier, service_type).await
        }
    }

    async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()> {
        // First check if pod exists with either service type to avoid unnecessary 404
        // Try both service types - one of them should have the pod
        let rcoder_exists = self
            .find_container(project_id, &ServiceType::WebAgentRunner)
            .await?
            .is_some();
        let computer_exists = self
            .find_container(project_id, &ServiceType::ComputerAgentRunner)
            .await?
            .is_some();

        if rcoder_exists {
            self.stop_container_by_identifier(project_id, &ServiceType::WebAgentRunner)
                .await?;
            info!(
                "[K8S] Pod for project {} deleted successfully (RCoder)",
                project_id
            );
            return Ok(());
        }

        if computer_exists {
            self.stop_container_by_identifier(project_id, &ServiceType::ComputerAgentRunner)
                .await?;
            info!(
                "[K8S] Pod for project {} deleted successfully (ComputerAgentRunner)",
                project_id
            );
            return Ok(());
        }

        // Pod doesn't exist - this is OK, consider it already stopped
        Ok(())
    }

    async fn stop_container_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        self.stop_container_by_identifier_inner(identifier, service_type)
            .await
    }

    async fn is_agent_image_drifted(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        self.is_agent_image_drifted_inner(identifier, service_type)
            .await
    }

    async fn is_container_running(&self, project_id: &str) -> ContainerRuntimeResult<bool> {
        Ok(self
            .find_container(project_id, &ServiceType::WebAgentRunner)
            .await?
            .map(|p| p.status == ContainerRuntimeStatus::Running)
            .unwrap_or(false))
    }

    async fn is_container_running_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self
            .find_container(identifier, service_type)
            .await?
            .map(|p| p.status == ContainerRuntimeStatus::Running)
            .unwrap_or(false))
    }

    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        self.list_containers_inner().await
    }

    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        self.sync_states_inner().await
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        self.cleanup_all_inner().await
    }

    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        // Try to list pods as a health check
        let lp = ListParams::default().limit(1);
        self.pods().list(&lp).await.map_err(|e| {
            ContainerRuntimeError::ConnectionError(format!("K8s health check failed: {}", e))
        })?;
        Ok(())
    }

    async fn restart_container_inplace(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        // 委派到 k8s_agent_pod 的 inherent 实现（沿用 get_deployment_status→get_app_status 的
        // 「委派→inherent」模式）。不委派则命中 trait 默认（NotImplemented）→ pod_restart 回落慢路径。
        self.restart_agent_container_inplace(identifier, service_type)
            .await
    }

    async fn diagnose_agent_pod(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<container_runtime_api::AgentPodDiagnostic> {
        self.diagnose_agent_pod_inner(identifier, service_type)
            .await
    }
}
