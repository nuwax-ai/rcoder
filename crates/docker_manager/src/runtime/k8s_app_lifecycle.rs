//! Userapp Deployment 生命周期(从 k8s_deployment.rs 拆出)。
//!
//! Scale/restart and removal of obsolete port resources. Identity-bound deletion
//! lives in k8s_app_deletion.

#[cfg(feature = "kubernetes")]
use container_runtime_api::{
    ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult, ExposeType,
};
#[cfg(feature = "kubernetes")]
use kube::api::{Patch, PatchParams};
#[cfg(feature = "kubernetes")]
use tracing::info;

#[cfg(feature = "kubernetes")]
use super::k8s_app_helpers::{
    IDLE_TIMEOUT_ANNOTATION, RECYCLE_ENABLED_ANNOTATION, WAKE_ON_TRAFFIC_ANNOTATION,
};
#[cfg(feature = "kubernetes")]
use super::k8s_deployment::APP_CONTAINER_NAME;

use super::kubernetes_runtime::KubernetesRuntime;

impl KubernetesRuntime {
    pub(super) async fn patch_captured_policy(
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

    pub(super) async fn capture_stop_target(
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
    pub(super) async fn capture_owned_app_identity(
        &self,
        context: &shared_types::UserAppExecutionContext,
        expected_version: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::AppResourceIdentity> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let name = self.app_deployment_name(&context.app_id);
        let deployment = self.deployments_api().get(&name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Read application mutation target: {error}"))
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

    pub(super) async fn restart_captured_target(
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

    pub(super) async fn start_captured_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_captured_with_policy(target, true, None).await
    }

    pub(super) async fn start_captured_management_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        self.start_captured_with_policy(target, false, None).await
    }

    pub(super) async fn start_captured_with_policy(
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

    pub(super) async fn stop_captured_target(
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

    pub(super) async fn prepare_captured_compute_start(
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

    pub(super) async fn prepare_captured_compute_retry(
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

    pub(super) async fn fence_captured_compute_write(
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

    pub(super) async fn start_captured_compute(
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

    pub(super) async fn reconcile_captured_compute_start(
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

    pub(super) async fn reconcile_captured_compute_stop(
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

    pub(super) async fn confirm_captured_compute_stopped(
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

    /// scale Deployment replicas
    pub async fn scale_app(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        if replicas < 0 {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Replica count must be nonnegative".into(),
            ));
        }
        let identity = self.capture_app_mutation_identity(app_id).await?;
        if replicas > 0 {
            self.claim_app_storage(app_id).await?;
        }
        let name = &identity.name;
        let patch = serde_json::json!({ "spec": { "replicas": replicas } });
        self.patch_captured_app(&identity, patch).await?;
        info!("[K8S-APP] Deployment {name} scaled to {replicas}");
        Ok(())
    }

    /// patch Deployment 的闲置回收策略注解(strategic merge:只改指定注解键,不碰 pod template → 不触发 rollout)。
    /// 字段 None=不改该键;两者皆 None 由上层 service 校验拒绝(此处防御性早返 Ok)。
    pub async fn patch_app_recycle_policy(
        &self,
        app_id: &str,
        recycle_enabled: Option<bool>,
        idle_timeout_seconds: Option<u64>,
    ) -> ContainerRuntimeResult<()> {
        let name = self.app_deployment_name(app_id);
        let mut ann = serde_json::Map::new();
        if let Some(b) = recycle_enabled {
            ann.insert(
                RECYCLE_ENABLED_ANNOTATION.to_string(),
                serde_json::Value::from(b.to_string()),
            );
        }
        if let Some(s) = idle_timeout_seconds {
            ann.insert(
                IDLE_TIMEOUT_ANNOTATION.to_string(),
                serde_json::Value::from(s.to_string()),
            );
        }
        if ann.is_empty() {
            return Ok(()); // 防御:两字段皆 None(service 层已校验)
        }
        let identity = self.capture_app_mutation_identity(app_id).await?;
        let patch = serde_json::json!({ "metadata": { "annotations": ann } });
        self.patch_captured_app(&identity, patch).await?;
        info!(
            "[K8S-APP] Deployment {name} recycle policy patched (enabled={:?}, idle_timeout={:?})",
            recycle_enabled, idle_timeout_seconds
        );
        Ok(())
    }

    pub async fn patch_app_wake_on_traffic(
        &self,
        app_id: &str,
        enabled: bool,
    ) -> ContainerRuntimeResult<()> {
        let identity = self.capture_app_mutation_identity(app_id).await?;
        let annotations =
            std::collections::BTreeMap::from([(WAKE_ON_TRAFFIC_ANNOTATION, enabled.to_string())]);
        let patch = serde_json::json!({ "metadata": { "annotations": annotations } });
        self.patch_captured_app(&identity, patch).await?;
        Ok(())
    }

    /// 触发滚动重启（rollout annotation）
    pub async fn restart_app(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let identity = self.capture_app_mutation_identity(app_id).await?;
        self.claim_app_storage(app_id).await?;
        let name = &identity.name;
        // kubectl 风格固定秒精度:注解值仅要求"变化即触发",但可变小数位
        // 是纳秒时间戳事故的同款模式(手拼时间戳一律定精度)。
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let patch = serde_json::json!({
            "spec": { "template": { "metadata": { "annotations": {
                "kubectl.kubernetes.io/restartedAt": now
            } } } }
        });
        self.patch_captured_app(&identity, patch).await?;
        info!("[K8S-APP] Deployment {name} restarted");
        Ok(())
    }

    /// Capture the physical target before any storage claim or workload mutation.
    async fn capture_app_mutation_identity(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<shared_types::AppResourceIdentity> {
        let name = self.app_deployment_name(app_id);
        let deployment = self.deployments_api().get(&name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Read application mutation target: {error}"))
        })?;
        let expected = self.build_app_labels(app_id, None, None);
        if !deployment.metadata.labels.as_ref().is_some_and(|labels| {
            expected
                .iter()
                .all(|(key, value)| labels.get(key) == Some(value))
        }) {
            return Err(ContainerRuntimeError::Conflict(
                "Application mutation target ownership changed".into(),
            ));
        }
        app_mutation_identity(name, &deployment.metadata)
    }

    pub(super) async fn patch_captured_app(
        &self,
        identity: &shared_types::AppResourceIdentity,
        patch: serde_json::Value,
    ) -> ContainerRuntimeResult<()> {
        let patch = condition_app_patch(identity, patch)?;
        self.deployments_api()
            .patch(
                &identity.name,
                &PatchParams::default(),
                &Patch::Merge(patch),
            )
            .await
            .map_err(|error| match &error {
                kube::Error::Api(response) if response.code == 409 => {
                    ContainerRuntimeError::Conflict(format!(
                        "Application mutation precondition failed: {error}"
                    ))
                }
                _ => super::builder_completion::k8s_error(
                    format!("Patch captured application: {error}"),
                    error,
                ),
            })?;
        Ok(())
    }

    /// Same identity-fenced patch as [`Self::patch_captured_app`] but with the
    /// strategic merge type: `containers` entries merge by container name
    /// instead of the array being replaced atomically.
    pub(super) async fn patch_captured_app_strategic(
        &self,
        identity: &shared_types::AppResourceIdentity,
        patch: serde_json::Value,
    ) -> ContainerRuntimeResult<()> {
        let patch = condition_app_patch(identity, patch)?;
        self.deployments_api()
            .patch(
                &identity.name,
                &PatchParams::default(),
                &Patch::Strategic(patch),
            )
            .await
            .map_err(|error| match &error {
                kube::Error::Api(response) if response.code == 409 => {
                    ContainerRuntimeError::Conflict(format!(
                        "Application mutation precondition failed: {error}"
                    ))
                }
                _ => super::builder_completion::k8s_error(
                    format!("Patch captured application: {error}"),
                    error,
                ),
            })?;
        Ok(())
    }

    /// Remove obsolete port resources after a successful update.
    pub async fn cleanup_orphan_port_resources(
        &self,
        app_id: &str,
        params: &ContainerCreateParams,
    ) -> ContainerRuntimeResult<()> {
        let has_http = params
            .ports
            .as_ref()
            .is_some_and(|ps| ps.iter().any(|p| p.expose_type == ExposeType::Http));
        let has_tcp = params
            .ports
            .as_ref()
            .is_some_and(|ps| ps.iter().any(|p| p.expose_type == ExposeType::Tcp));
        let has_env = params.env.as_ref().is_some_and(|e| !e.is_empty());
        let has_secrets = params.secrets.as_ref().is_some_and(|s| !s.is_empty());
        // step-D 写面 fencing：清理删除一律 uid+RV 前置（对齐 delete_captured）
        // ——接管后的迟到删除被前置拒绝，不会误删同名新代资源。
        if !has_http {
            let name = self.app_http_route_name(app_id);
            let api = self.httproute_api();
            if let Some(live) = api.get_opt(&name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get httproute {name} before cleanup: {e}"))
            })? {
                let dp =
                    super::k8s_runtime_helpers::conditioned_delete_params(&live.metadata, None)?;
                self.ignore_404(api.delete(&name, &dp).await).await?;
            }
        }
        if !has_tcp {
            let name = self.app_nodeport_name(app_id);
            let api = self.services_api();
            if let Some(live) = api.get_opt(&name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get nodeport {name} before cleanup: {e}"))
            })? {
                let dp =
                    super::k8s_runtime_helpers::conditioned_delete_params(&live.metadata, None)?;
                self.ignore_404(api.delete(&name, &dp).await).await?;
            }
        }
        if !has_env {
            let name = self.app_config_name(app_id);
            let api = self.configmaps_api();
            if let Some(live) = api.get_opt(&name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get configmap {name} before cleanup: {e}"))
            })? {
                let dp =
                    super::k8s_runtime_helpers::conditioned_delete_params(&live.metadata, None)?;
                self.ignore_404(api.delete(&name, &dp).await).await?;
            }
        }
        if !has_secrets {
            let name = self.app_secret_name(app_id);
            let api = self.secrets_api();
            if let Some(live) = api.get_opt(&name).await.map_err(|e| {
                ContainerRuntimeError::K8sError(format!("get secret {name} before cleanup: {e}"))
            })? {
                let dp =
                    super::k8s_runtime_helpers::conditioned_delete_params(&live.metadata, None)?;
                self.ignore_404(api.delete(&name, &dp).await).await?;
            }
        }
        Ok(())
    }

    /// 等 app Pod 容器全部退出（按 rcoder.io/app-id label 轮询 Pod phase），best-effort：
    /// 容器退出（phase != Running）或 Pod 消失即返回；超时/API 错误仅 warn 不阻塞删除
    /// （app 复用共享 PVC 子目录，残留写入影响可控）。
    /// 仅容忍 404（视为已删除/幂等），其余 K8s 错误透传
    async fn ignore_404<T>(&self, r: Result<T, kube::Error>) -> ContainerRuntimeResult<()> {
        match r {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
            Err(e) => Err(ContainerRuntimeError::K8sError(format!("delete: {e}"))),
        }
    }
}

fn app_mutation_identity(
    name: String,
    metadata: &k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
) -> ContainerRuntimeResult<shared_types::AppResourceIdentity> {
    if metadata.deletion_timestamp.is_some() || metadata.name.as_ref() != Some(&name) {
        return Err(ContainerRuntimeError::Conflict(
            "Application mutation target is deleting or changed".into(),
        ));
    }
    let uid = metadata
        .uid
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "Application mutation target has no UID".into(),
            )
        })?;
    let version = metadata
        .resource_version
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "Application mutation target has no resource version".into(),
            )
        })?;
    Ok(shared_types::AppResourceIdentity {
        kind: shared_types::AppResourceKind::Deployment,
        name,
        uid: uid.into(),
        resource_version: Some(version.into()),
    })
}

/// Restart patch body: wake annotation + replicas=1 + per-operation template
/// annotation (guarantees a rollout even when the image is unchanged). With an
/// image the pod-template container image joins the same write — the caller
/// must dispatch via strategic merge (containers merge by name; an RFC 7386
/// merge patch would replace the array atomically).
fn app_restart_patch(operation_id: &str, image: Option<&str>) -> serde_json::Value {
    let template = match image {
        Some(image) => serde_json::json!({
            "metadata":{"annotations":{"rcoder.io/restart-operation":operation_id}},
            "spec":{"containers":[{"name":APP_CONTAINER_NAME,"image":image}]}
        }),
        None => serde_json::json!({
            "metadata":{"annotations":{"rcoder.io/restart-operation":operation_id}}
        }),
    };
    serde_json::json!({
        "metadata":{"annotations":{(WAKE_ON_TRAFFIC_ANNOTATION):"true"}},
        "spec":{"replicas":1,"template":template}
    })
}

/// Compute-start patch body: wake + start receipt annotations + replicas=1,
/// optionally rolling the pod-template container image in the same single
/// write (strategic merge required, same as [`app_restart_patch`]).
fn app_compute_start_patch(receipt: &str, image: Option<&str>) -> serde_json::Value {
    match image {
        Some(image) => serde_json::json!({
            "metadata":{"annotations":{
                (WAKE_ON_TRAFFIC_ANNOTATION):"true",
                "rcoder.io/compute-start-receipt":receipt
            }},
            "spec":{"replicas":1,"template":{"spec":{"containers":[
                {"name":APP_CONTAINER_NAME,"image":image}
            ]}}}
        }),
        None => serde_json::json!({
            "metadata":{"annotations":{
                (WAKE_ON_TRAFFIC_ANNOTATION):"true",
                "rcoder.io/compute-start-receipt":receipt
            }},
            "spec":{"replicas":1}
        }),
    }
}

fn condition_app_patch(
    identity: &shared_types::AppResourceIdentity,
    mut patch: serde_json::Value,
) -> ContainerRuntimeResult<serde_json::Value> {
    let version = identity
        .resource_version
        .as_deref()
        .filter(|value| !value.is_empty());
    if identity.kind != shared_types::AppResourceKind::Deployment
        || identity.uid.is_empty()
        || identity.name.is_empty()
        || version.is_none()
    {
        return Err(ContainerRuntimeError::ConfigurationError(
            "Incomplete application mutation identity".into(),
        ));
    }
    let object = patch.as_object_mut().ok_or_else(|| {
        ContainerRuntimeError::ConfigurationError("Application patch must be an object".into())
    })?;
    let metadata = object
        .entry("metadata")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            ContainerRuntimeError::ConfigurationError(
                "Application patch metadata must be an object".into(),
            )
        })?;
    metadata.insert("uid".into(), identity.uid.clone().into());
    metadata.insert("resourceVersion".into(), serde_json::json!(version));
    Ok(patch)
}

#[cfg(test)]
mod mutation_identity_tests {
    use super::*;

    /// 镜像滚动版 restart patch：同一写携带镜像+重启注解+replicas；
    /// containers 只含 name/image 两个键（Strategic merge-by-name 语义，
    /// 误用 Merge patch 会整组替换 containers——此处固化形状防回退）。
    #[test]
    fn restart_patch_carries_image_annotation_and_replicas_in_one_write() {
        let patch = app_restart_patch("op-restart", Some("registry.test/app-runtime:0.2.0"));
        assert_eq!(
            patch["metadata"]["annotations"][WAKE_ON_TRAFFIC_ANNOTATION],
            "true"
        );
        assert_eq!(patch["spec"]["replicas"], 1);
        assert_eq!(
            patch["spec"]["template"]["metadata"]["annotations"]["rcoder.io/restart-operation"],
            "op-restart"
        );
        let containers = patch["spec"]["template"]["spec"]["containers"]
            .as_array()
            .expect("containers array");
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0]["name"], APP_CONTAINER_NAME);
        assert_eq!(containers[0]["image"], "registry.test/app-runtime:0.2.0");
        assert_eq!(
            containers[0].as_object().expect("container object").len(),
            2,
            "container entry must carry only name+image"
        );
    }

    /// 无镜像版保持既有形状：template 只含 metadata 注解，不出现 containers
    /// 键（走 Merge patch 的判定依据）。
    #[test]
    fn restart_patch_without_image_keeps_plain_shape() {
        let patch = app_restart_patch("op-restart", None);
        assert_eq!(patch["spec"]["replicas"], 1);
        assert_eq!(
            patch["spec"]["template"]["metadata"]["annotations"]["rcoder.io/restart-operation"],
            "op-restart"
        );
        assert!(
            patch["spec"]["template"].get("spec").is_none(),
            "no container template spec without an image roll"
        );
    }

    #[test]
    fn compute_start_patch_carries_image_only_when_present() {
        let plain = app_compute_start_patch("{\"op\":\"a\"}", None);
        assert_eq!(plain["spec"]["replicas"], 1);
        assert_eq!(
            plain["metadata"]["annotations"]["rcoder.io/compute-start-receipt"],
            "{\"op\":\"a\"}"
        );
        assert!(plain["spec"].get("template").is_none());

        let rolled = app_compute_start_patch("{\"op\":\"b\"}", Some("registry.test/app-runtime:9"));
        let containers = rolled["spec"]["template"]["spec"]["containers"]
            .as_array()
            .expect("containers array");
        assert_eq!(containers[0]["name"], APP_CONTAINER_NAME);
        assert_eq!(containers[0]["image"], "registry.test/app-runtime:9");
        assert_eq!(
            rolled["metadata"]["annotations"][WAKE_ON_TRAFFIC_ANNOTATION],
            "true"
        );
    }

    #[test]
    fn conditional_workload_patch_retains_requested_changes_and_captured_identity() {
        let metadata = k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some("rcoder-app-one".into()),
            uid: Some("original-uid".into()),
            resource_version: Some("17".into()),
            ..Default::default()
        };
        let identity = app_mutation_identity("rcoder-app-one".into(), &metadata).expect("identity");
        for change in [
            serde_json::json!({"spec":{"replicas":0}}),
            serde_json::json!({"metadata":{"annotations":{"rcoder.io/wake-on-traffic":"false"}}}),
            serde_json::json!({"spec":{"template":{"metadata":{"annotations":{"restart":"now"}}}}}),
        ] {
            let patch = condition_app_patch(&identity, change.clone()).expect("patch");
            assert_eq!(patch["metadata"]["uid"], "original-uid");
            assert_eq!(patch["metadata"]["resourceVersion"], "17");
            if let Some(spec) = change.get("spec") {
                assert_eq!(&patch["spec"], spec);
            }
            if let Some(annotations) = change.pointer("/metadata/annotations") {
                assert_eq!(&patch["metadata"]["annotations"], annotations);
            }
        }
        let mut missing = metadata.clone();
        missing.uid = None;
        assert!(app_mutation_identity("rcoder-app-one".into(), &missing).is_err());
        let mut missing = metadata;
        missing.resource_version = None;
        assert!(app_mutation_identity("rcoder-app-one".into(), &missing).is_err());
        assert!(app_mutation_identity("replacement-name".into(), &missing).is_err());
    }
}
