use super::*;

impl KubernetesRuntime {
    pub(crate) async fn patch_captured_policy(
        &self,
        target: &shared_types::UserAppMutationTarget,
        policy: &shared_types::UserAppRuntimePolicy,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Deployment
            || target.resource.name != self.app_deployment_name(&target.context.app_id)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid captured policy target".into(),
            ));
        }
        let mut annotations = serde_json::Map::new();
        if let Some(value) = policy.recycle_enabled {
            annotations.insert(RECYCLE_ENABLED_ANNOTATION.into(), value.to_string().into());
        }
        if let Some(value) = policy.idle_timeout_seconds {
            annotations.insert(IDLE_TIMEOUT_ANNOTATION.into(), value.to_string().into());
        }
        if let Some(value) = policy.wake_on_traffic {
            annotations.insert(WAKE_ON_TRAFFIC_ANNOTATION.into(), value.to_string().into());
        }
        if annotations.is_empty() {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Policy patch is empty".into(),
            ));
        }
        self.patch_captured_app(
            &target.resource,
            serde_json::json!({"metadata":{"annotations":annotations}}),
        )
        .await
    }

    pub(crate) async fn capture_stop_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_version: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::UserAppMutationTarget> {
        let resource = self
            .capture_owned_app_identity(context, expected_version)
            .await?;
        Ok(shared_types::UserAppMutationTarget {
            context: context.clone(),
            resource,
        })
    }

    /// Capture lifecycle ownership once, before staging any replacement config.
    pub(crate) async fn capture_owned_app_identity(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_version: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::AppResourceIdentity> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let name = self.app_deployment_name(&context.app_id);
        let deployment = self.deployments_api().get(&name).await.map_err(|error| {
            if matches!(&error, kube::Error::Api(response) if response.code == 404) {
                ContainerRuntimeError::ContainerNotFound(name.clone())
            } else {
                ContainerRuntimeError::K8sError(format!(
                    "Read application mutation target: {error}"
                ))
            }
        })?;
        let expected_labels = self.build_app_labels(&context.app_id, None, None);
        if !deployment.metadata.labels.as_ref().is_some_and(|labels| {
            expected_labels
                .iter()
                .all(|(key, value)| labels.get(key) == Some(value))
        }) {
            return Err(ContainerRuntimeError::Conflict(
                "Application mutation target ownership changed".into(),
            ));
        }
        let annotations = deployment.metadata.annotations.as_ref().ok_or_else(|| {
            ContainerRuntimeError::Conflict(
                "Application mutation target requires lifecycle adoption".into(),
            )
        })?;
        context
            .validate_application_metadata(annotations)
            .map_err(ContainerRuntimeError::Conflict)?;
        let resource = app_mutation_identity(name, &deployment.metadata)?;
        if expected_version
            .is_some_and(|version| resource.resource_version.as_deref() != Some(version))
        {
            return Err(ContainerRuntimeError::Conflict(
                "Application mutation target version changed".into(),
            ));
        }
        Ok(resource)
    }

    pub(crate) async fn restart_captured_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
        image: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Deployment
            || target.resource.name != self.app_deployment_name(&target.context.app_id)
            || target.resource.uid.is_empty()
            || target
                .resource
                .resource_version
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid captured application restart target".into(),
            ));
        }
        self.claim_app_storage_with_context(&target.context.app_id, Some(&target.context))
            .await?;
        let patch = app_restart_patch(&target.context.operation_id, image);
        if image.is_some() {
            // containers merge-by-name requires the strategic patch type; an
            // RFC 7386 merge patch would replace the array atomically.
            self.patch_captured_app_strategic(&target.resource, patch)
                .await
        } else {
            self.patch_captured_app(&target.resource, patch).await
        }
    }

    pub(crate) async fn start_captured_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_captured_with_policy(target, true, None).await
    }

    pub(crate) async fn start_captured_management_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_captured_with_policy(target, false, None).await
    }

    pub(crate) async fn start_captured_with_policy(
        &self,
        target: &shared_types::UserAppMutationTarget,
        enable_traffic_wake: bool,
        image: Option<&str>,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Deployment
            || target.resource.name != self.app_deployment_name(&target.context.app_id)
            || target.resource.uid.is_empty()
            || target
                .resource
                .resource_version
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid captured application start target".into(),
            ));
        }
        self.claim_app_storage_with_context(&target.context.app_id, Some(&target.context))
            .await?;
        let receipt = serde_json::to_string(&target.context).map_err(|error| {
            ContainerRuntimeError::ConfigurationError(format!("Encode start receipt: {error}"))
        })?;
        let patch = if enable_traffic_wake {
            app_compute_start_patch(&receipt, image)
        } else {
            serde_json::json!({"metadata":{"annotations":{"rcoder.io/compute-start-receipt":receipt}},"spec":{"replicas":1}})
        };
        if image.is_some() {
            self.patch_captured_app_strategic(&target.resource, patch)
                .await
        } else {
            self.patch_captured_app(&target.resource, patch).await
        }
    }

    pub(crate) async fn stop_captured_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
        wake_on_traffic: bool,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.name != self.app_deployment_name(&target.context.app_id) {
            return Err(ContainerRuntimeError::Conflict(
                "Application stop target name changed".into(),
            ));
        }
        let receipt = serde_json::to_string(&target.context).map_err(|error| {
            ContainerRuntimeError::ConfigurationError(format!("Encode stop receipt: {error}"))
        })?;
        self.patch_captured_app(&target.resource, serde_json::json!({
            "metadata":{"annotations":{(WAKE_ON_TRAFFIC_ANNOTATION):wake_on_traffic.to_string(), "rcoder.io/compute-stop-receipt":receipt}},
            "spec":{"replicas":0}
        })).await
    }

    pub(crate) async fn prepare_captured_compute_start(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<shared_types::UserAppComputeStartTarget> {
        use k8s_openapi::api::core::v1::PersistentVolumeClaim;
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let deployment = self
            .deployments_api()
            .get(&target.resource.name)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("Read restart workload: {e}")))?;
        if target.resource.kind != shared_types::AppResourceKind::Deployment
            || target.resource.name != self.app_deployment_name(&target.context.app_id)
            || deployment.metadata.uid.as_deref() != Some(target.resource.uid.as_str())
            || deployment.metadata.resource_version != target.resource.resource_version
        {
            return Err(ContainerRuntimeError::Conflict(
                "Restart workload changed before volume capture".into(),
            ));
        }
        let annotations = deployment.metadata.annotations.as_ref().ok_or_else(|| {
            ContainerRuntimeError::Conflict("Restart workload identity missing".into())
        })?;
        target
            .context
            .validate_application_metadata(annotations)
            .map_err(ContainerRuntimeError::Conflict)?;
        let spec = deployment.spec.as_ref().ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError("Restart workload spec missing".into())
        })?;
        let pvc_api: kube::Api<PersistentVolumeClaim> =
            kube::Api::namespaced(self.client.clone(), &self.namespace);
        let mut volumes = Vec::new();
        for volume in spec
            .template
            .spec
            .as_ref()
            .and_then(|spec| spec.volumes.as_ref())
            .into_iter()
            .flatten()
        {
            let Some(claim) = volume.persistent_volume_claim.as_ref() else {
                continue;
            };
            let pvc = pvc_api.get(&claim.claim_name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("Read restart volume: {e}"))
            })?;
            if pvc.metadata.deletion_timestamp.is_some() {
                return Err(ContainerRuntimeError::Conflict(
                    "Restart volume is deleting".into(),
                ));
            }
            let annotations = pvc.metadata.annotations.as_ref().ok_or_else(|| {
                ContainerRuntimeError::Conflict("Restart volume lifecycle is missing".into())
            })?;
            target
                .context
                .validate_application_metadata(annotations)
                .map_err(ContainerRuntimeError::Conflict)?;
            volumes.push(shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::PersistentVolumeClaim,
                name: claim.claim_name.clone(),
                uid: pvc.metadata.uid.ok_or_else(|| {
                    ContainerRuntimeError::ConfigurationError("Restart volume UID missing".into())
                })?,
                resource_version: pvc.metadata.resource_version,
            });
        }
        if volumes.is_empty() {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Restart has no captured workspace volume".into(),
            ));
        }
        Ok(shared_types::UserAppComputeStartTarget {
            target: target.clone(),
            compute_start_single_write: true,
            volumes,
            restart_image: None,
        })
    }

    pub(crate) async fn prepare_captured_compute_retry(
        &self,
        captured: &shared_types::UserAppComputeStartTarget,
    ) -> ContainerRuntimeResult<Option<shared_types::UserAppComputeStartTarget>> {
        if !captured.compute_start_single_write {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Original single-write witness is required".into(),
            ));
        }
        let target = &captured.target;
        let deployment = self
            .deployments_api()
            .get(&target.resource.name)
            .await
            .map_err(|e| {
                ContainerRuntimeError::K8sError(format!("Observe original compute start: {e}"))
            })?;
        if deployment.metadata.uid.as_deref() != Some(target.resource.uid.as_str()) {
            return Err(ContainerRuntimeError::Conflict(
                "Original restart workload was replaced".into(),
            ));
        }
        let applied = deployment
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get("rcoder.io/compute-start-receipt"))
            .map(|value| serde_json::from_str::<shared_types::UserAppExecutionContext>(value))
            .transpose()
            .map_err(|_| {
                ContainerRuntimeError::ConfigurationError("Invalid compute start receipt".into())
            })?;
        if applied.as_ref() == Some(&target.context) {
            return Ok(None);
        }
        if !self.reconcile_captured_compute_stop(target).await? {
            return Err(ContainerRuntimeError::Conflict(
                "Original restart stop is not confirmed".into(),
            ));
        }
        // Use the version from before the stop observation. If a start raced
        // with those reads, preparation rejects that version instead of rebasing.
        let mut refreshed = target.clone();
        refreshed.resource.resource_version = deployment.metadata.resource_version;
        if refreshed
            .resource
            .resource_version
            .as_deref()
            .is_none_or(str::is_empty)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Restart workload version missing".into(),
            ));
        }
        let mut prepared = self.prepare_captured_compute_start(&refreshed).await?;
        prepared.restart_image = captured.restart_image.clone();
        captured
            .verify_same_volumes(&prepared)
            .map_err(ContainerRuntimeError::Conflict)?;
        Ok(Some(prepared))
    }

    pub(crate) async fn fence_captured_compute_write(
        &self,
        captured: &shared_types::UserAppComputeStartTarget,
    ) -> ContainerRuntimeResult<bool> {
        let target = &captured.target;
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if !captured.compute_start_single_write
            || captured.volumes.is_empty()
            || target.resource.kind != shared_types::AppResourceKind::Deployment
            || target.resource.name != self.app_deployment_name(&target.context.app_id)
            || target
                .resource
                .resource_version
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Conditional compute fencing requires the original single-write witness".into(),
            ));
        }
        let deployment = self
            .deployments_api()
            .get(&target.resource.name)
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Read compute fence target: {error}"))
            })?;
        if deployment.metadata.uid.as_deref() != Some(target.resource.uid.as_str()) {
            return Err(ContainerRuntimeError::Conflict(
                "Compute fence workload was replaced".into(),
            ));
        }
        let annotations = deployment.metadata.annotations.as_ref().ok_or_else(|| {
            ContainerRuntimeError::Conflict("Compute fence lifecycle missing".into())
        })?;
        target
            .context
            .validate_application_metadata(annotations)
            .map_err(ContainerRuntimeError::Conflict)?;
        if deployment
            .metadata
            .resource_version
            .as_deref()
            .is_none_or(str::is_empty)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Compute fence version missing".into(),
            ));
        }
        if deployment.metadata.resource_version != target.resource.resource_version {
            // The original write either committed already or lost its exact
            // precondition. No rebase is allowed after the operation is superseded.
            return Ok(true);
        }
        // Consume the old version without touching replicas, Pod template or PVC.
        // A racing original compute and this fence cannot both win the same RV.
        let marker = serde_json::to_string(&target.context).map_err(|error| {
            ContainerRuntimeError::ConfigurationError(format!("Encode compute fence: {error}"))
        })?;
        self.patch_captured_app(
            &target.resource,
            serde_json::json!({
                "metadata":{"annotations":{"rcoder.io/compute-start-fence":marker}}
            }),
        )
        .await?;
        // Verify a changed version even if the API considered the patch a no-op.
        let after = self
            .deployments_api()
            .get(&target.resource.name)
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Verify compute fence: {error}"))
            })?;
        Ok(
            after.metadata.uid.as_deref() == Some(target.resource.uid.as_str())
                && after
                    .metadata
                    .resource_version
                    .as_deref()
                    .is_some_and(|v| !v.is_empty())
                && after.metadata.resource_version != target.resource.resource_version,
        )
    }

    pub(crate) async fn start_captured_compute(
        &self,
        captured: &shared_types::UserAppComputeStartTarget,
    ) -> ContainerRuntimeResult<()> {
        if !captured.compute_start_single_write {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Single-write restart witness required".into(),
            ));
        }
        self.confirm_captured_compute_stopped(&captured.target)
            .await?;
        let fresh = self
            .prepare_captured_compute_start(&captured.target)
            .await?;
        if fresh.volumes.len() != captured.volumes.len()
            || fresh
                .volumes
                .iter()
                .zip(&captured.volumes)
                .any(|(a, b)| a.kind != b.kind || a.name != b.name || a.uid != b.uid)
        {
            return Err(ContainerRuntimeError::Conflict(
                "Restart volume identity changed".into(),
            ));
        }
        let receipt = serde_json::to_string(&captured.target.context).map_err(|e| {
            ContainerRuntimeError::ConfigurationError(format!("Encode compute start receipt: {e}"))
        })?;
        // The caller already drained prior writers and owns the operation lease.
        // No PVC mutation, create, or second runtime write follows this patch.
        let patch = app_compute_start_patch(&receipt, captured.restart_image.as_deref());
        if captured.restart_image.is_some() {
            self.patch_captured_app_strategic(&captured.target.resource, patch)
                .await
        } else {
            self.patch_captured_app(&captured.target.resource, patch)
                .await
        }
    }

    pub(crate) async fn reconcile_captured_compute_start(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<bool> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Deployment
            || target.resource.uid.is_empty()
            || target.resource.name != self.app_deployment_name(&target.context.app_id)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid start recovery target".into(),
            ));
        }
        let Some(deployment) = self
            .deployments_api()
            .get_opt(&target.resource.name)
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Read start receipt: {error}"))
            })?
        else {
            return Ok(false);
        };
        let receipt = deployment
            .metadata
            .annotations
            .as_ref()
            .and_then(|values| values.get("rcoder.io/compute-start-receipt"))
            .and_then(|value| {
                serde_json::from_str::<shared_types::UserAppExecutionContext>(value).ok()
            });
        if deployment.metadata.uid.as_deref() != Some(target.resource.uid.as_str())
            || receipt.as_ref() != Some(&target.context)
            || !deployment
                .spec
                .as_ref()
                .is_some_and(|spec| spec.replicas == Some(1))
            || !deployment.status.as_ref().is_some_and(|status| {
                status.updated_replicas.unwrap_or(0) > 0
                    && deployment.metadata.generation.is_some_and(|generation| {
                        status
                            .observed_generation
                            .is_some_and(|observed| observed >= generation)
                    })
            })
        {
            return Ok(false);
        }
        // Physical restart ends when this Deployment's current Pod is running.
        // ready_replicas may depend on the user's HTTP readiness probe; a
        // failing application must not hold the compute operation forever.
        self.current_app_compute_pod_running(&deployment).await
    }

    async fn current_app_compute_pod_running(
        &self,
        deployment: &k8s_openapi::api::apps::v1::Deployment,
    ) -> ContainerRuntimeResult<bool> {
        use k8s_openapi::api::{apps::v1::ReplicaSet, core::v1::Pod};
        use kube::{Api, api::ListParams};

        let Some(deployment_uid) = deployment.metadata.uid.as_deref() else {
            return Ok(false);
        };
        let Some(revision) = deployment
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get("deployment.kubernetes.io/revision"))
        else {
            return Ok(false);
        };
        let Some(selector) = deployment.spec.as_ref().map(|spec| &spec.selector) else {
            return Ok(false);
        };
        if selector
            .match_expressions
            .as_ref()
            .is_some_and(|expressions| !expressions.is_empty())
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Unsupported application selector during compute observation".into(),
            ));
        }
        let Some(labels) = selector
            .match_labels
            .as_ref()
            .filter(|labels| !labels.is_empty())
        else {
            return Ok(false);
        };
        let label_selector = labels
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(",");
        let params = ListParams::default().labels(&label_selector);
        let replica_sets: Api<ReplicaSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let current_rs_uids: std::collections::HashSet<String> = replica_sets
            .list(&params)
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Observe compute ReplicaSets: {error}"))
            })?
            .items
            .into_iter()
            .filter(|rs| {
                rs.metadata.deletion_timestamp.is_none()
                    && rs.metadata.owner_references.as_ref().is_some_and(|owners| {
                        owners.iter().any(|owner| owner.uid == deployment_uid)
                    })
                    && rs.metadata.annotations.as_ref().is_some_and(|annotations| {
                        annotations.get("deployment.kubernetes.io/revision") == Some(revision)
                    })
                    && rs
                        .spec
                        .as_ref()
                        .is_some_and(|spec| spec.replicas.unwrap_or(0) > 0)
            })
            .filter_map(|rs| rs.metadata.uid)
            .collect();
        if current_rs_uids.is_empty() {
            return Ok(false);
        }
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        Ok(pods
            .list(&params)
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Observe compute Pods: {error}"))
            })?
            .items
            .iter()
            .any(|pod| {
                pod.metadata.deletion_timestamp.is_none()
                    && pod
                        .metadata
                        .owner_references
                        .as_ref()
                        .is_some_and(|owners| {
                            owners
                                .iter()
                                .any(|owner| current_rs_uids.contains(&owner.uid))
                        })
                    && pod.status.as_ref().is_some_and(|status| {
                        status
                            .container_statuses
                            .as_ref()
                            .is_some_and(|containers| {
                                containers.iter().any(|container| {
                                    container.name == APP_CONTAINER_NAME
                                        && container
                                            .state
                                            .as_ref()
                                            .is_some_and(|state| state.running.is_some())
                                })
                            })
                    })
            }))
    }

    pub(crate) async fn reconcile_captured_compute_stop(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<bool> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Deployment
            || target.resource.name != self.app_deployment_name(&target.context.app_id)
            || target.resource.uid.is_empty()
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid stop recovery target".into(),
            ));
        }
        let matches = |deployment: &k8s_openapi::api::apps::v1::Deployment| {
            deployment.metadata.uid.as_deref() == Some(target.resource.uid.as_str())
                && deployment
                    .spec
                    .as_ref()
                    .is_some_and(|spec| spec.replicas == Some(0))
                && deployment
                    .metadata
                    .annotations
                    .as_ref()
                    .and_then(|a| a.get("rcoder.io/compute-stop-receipt"))
                    .and_then(|s| {
                        serde_json::from_str::<shared_types::UserAppExecutionContext>(s).ok()
                    })
                    .is_some_and(|context| context == target.context)
        };
        let deployments = self.deployments_api();
        let before = deployments
            .get_opt(&target.resource.name)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("Read stop receipt: {e}")))?;
        if !before.as_ref().is_some_and(&matches) {
            return Ok(false);
        }
        self.confirm_captured_compute_stopped(target).await?;
        let after = deployments
            .get_opt(&target.resource.name)
            .await
            .map_err(|e| ContainerRuntimeError::K8sError(format!("Recheck stop receipt: {e}")))?;
        Ok(after.as_ref().is_some_and(matches))
    }

    pub(crate) async fn confirm_captured_compute_stopped(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        if target.resource.kind != shared_types::AppResourceKind::Deployment
            || target.resource.uid.is_empty()
            || target.resource.name != self.app_deployment_name(&target.context.app_id)
        {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Invalid captured stop confirmation target".into(),
            ));
        }

        use k8s_openapi::api::{apps::v1::Deployment, core::v1::Pod};
        let deployments: kube::Api<Deployment> =
            kube::Api::namespaced(self.client.clone(), &self.namespace);
        let pods: kube::Api<Pod> = kube::Api::namespaced(self.client.clone(), &self.namespace);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let deployment = deployments.get(&target.resource.name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("Confirm stopped deployment: {e}"))
            })?;
            if deployment.metadata.uid.as_deref() != Some(target.resource.uid.as_str()) {
                return Err(ContainerRuntimeError::Conflict(
                    "Stopped deployment identity changed".into(),
                ));
            }
            let spec = deployment.spec.as_ref().ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError("Deployment spec missing".into())
            })?;
            if spec.replicas != Some(0) {
                return Err(ContainerRuntimeError::Conflict(
                    "Deployment no longer has a stop intent".into(),
                ));
            }
            // Generated application selectors are exact matchLabels. Refuse an
            // unsupported selector rather than accidentally overlooking old Pods.
            if spec
                .selector
                .match_expressions
                .as_ref()
                .is_some_and(|v| !v.is_empty())
            {
                return Err(ContainerRuntimeError::ConfigurationError(
                    "Unsupported application selector".into(),
                ));
            }
            let labels = spec
                .selector
                .match_labels
                .as_ref()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    ContainerRuntimeError::ConfigurationError("Application selector missing".into())
                })?;
            let selector = labels
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",");
            let remaining = pods
                .list(&kube::api::ListParams::default().labels(&selector))
                .await
                .map_err(|e| {
                    ContainerRuntimeError::K8sError(format!("Confirm old Pods exited: {e}"))
                })?;
            if remaining.items.is_empty() {
                let confirmed = deployments.get(&target.resource.name).await.map_err(|e| {
                    ContainerRuntimeError::K8sError(format!("Recheck stopped deployment: {e}"))
                })?;
                if confirmed.metadata.uid == deployment.metadata.uid
                    && confirmed.metadata.resource_version == deployment.metadata.resource_version
                {
                    return Ok(());
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ContainerRuntimeError::Conflict(
                    "Old compute exit remains unconfirmed".into(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
}
