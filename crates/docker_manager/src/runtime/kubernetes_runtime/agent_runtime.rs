use super::*;

#[cfg(feature = "kubernetes")]
#[async_trait]
impl AgentContainerRuntime for KubernetesRuntime {
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
        self.release_captured_application_operation(context, &receipt.lease)
            .await?;
        Ok(Some(shared_types::BuilderCreationEvidence {
            creation_lease_released: true,
            target: receipt.target,
            container: receipt.container,
        }))
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
                let result = if params.resource_binding.is_some() {
                    runtime
                        .resume_bound_builder(&params)
                        .await
                        // RV07：本操作内执行了受控物理替换时，最终捕获与
                        // 收据改用新物理绑定——旧 UID 绑定对替换后的 STS
                        // 必然失配，不能让升级成功后收据失败。
                        .map(|(info, upgraded)| (info, upgraded.or(binding.clone())))
                } else {
                    runtime
                        .create_agent_container_with_creation_receipt(params, lease.receipt())
                        .await
                        .map(|info| (info, binding.clone()))
                };
                let result = match (result, context.as_ref()) {
                    (Ok((info, effective_binding)), Some(context)) => {
                        let recorded = async {
                            let target = runtime
                                .capture_builder_compute_with_binding(
                                    context,
                                    effective_binding.as_ref(),
                                    false,
                                )
                                .await?;
                            let receipt = crate::runtime::builder_creation_receipt::BuilderCreationReceipt {
                                target,
                                container: info.clone(),
                                lease: lease.receipt().ok_or_else(|| {
                                    ContainerRuntimeError::ConfigurationError(
                                        "Builder creation lease receipt missing".into(),
                                    )
                                })?,
                            };
                            if effective_binding.is_some() {
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
