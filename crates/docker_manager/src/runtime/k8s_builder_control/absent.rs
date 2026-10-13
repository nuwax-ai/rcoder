//! Restore missing compute under its admitted operation, retaining actual data.
use super::super::k8s_pvc::K8sPvcOps as _;
use super::*;
use container_runtime_api::ContainerCreateParams;
use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use shared_types::UserAppOperationLeaseReceipt;

impl KubernetesRuntime {
    async fn retained_builder_volume(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<AppResourceIdentity> {
        let name = self.workspace_pvc_name(&target.context.app_id, &ServiceType::UserappBuilder)?;
        let api: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), &self.namespace);
        let pvc = api
            .get(&name)
            .await
            .map_err(|error| api_error("Observe retained builder workspace", error))?;
        let metadata = &pvc.metadata;
        if metadata.name.as_deref() != Some(name.as_str())
            || metadata
                .namespace
                .as_ref()
                .is_some_and(|namespace| namespace != &self.namespace)
            || metadata.deletion_timestamp.is_some()
            || metadata
                .resource_version
                .as_deref()
                .is_none_or(str::is_empty)
            || metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("service_type"))
                .map(String::as_str)
                != Some(ServiceType::UserappBuilder.to_string().as_str())
        {
            return Err(Error::Conflict(
                "Retained builder PVC scope or ownership changed".into(),
            ));
        }
        let annotations = metadata.annotations.as_ref().ok_or_else(|| {
            Error::Conflict("Retained builder PVC application identity missing".into())
        })?;
        // Old creators and request digests are diagnostic. Only the current
        // application/lifecycle and actual immutable volume UID bind its data.
        target
            .context
            .validate_application_metadata(annotations)
            .map_err(Error::Conflict)?;
        let uid = metadata
            .uid
            .as_ref()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| Error::Conflict("Retained builder PVC UID missing".into()))?
            .clone();
        Ok(AppResourceIdentity {
            kind: AppResourceKind::PersistentVolumeClaim,
            name,
            uid,
            resource_version: None,
        })
    }

    pub(super) async fn capture_absent_builder_volumes(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Vec<AppResourceIdentity>> {
        target.validate().map_err(Error::ConfigurationError)?;
        if target.workload.is_some() || target.pod.is_some() || target.resource_binding.is_some() {
            return Err(Error::Conflict(
                "Absent builder witness has a physical compute target".into(),
            ));
        }
        self.confirm_builder_compute_absent(&target.context).await?;
        let before = self.retained_builder_volume(target).await?;
        self.confirm_builder_compute_absent(&target.context).await?;
        let after = self.retained_builder_volume(target).await?;
        if before != after {
            return Err(Error::Conflict(
                "Retained builder workspace changed during absence capture".into(),
            ));
        }
        Ok(vec![after])
    }

    async fn original_absent_creation_receipt(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Option<super::super::builder_creation_receipt::BuilderCreationReceipt>> {
        let receipt = match self.read_builder_creation_receipt(&target.context).await? {
            Some(receipt) => Some(receipt),
            None => self.read_builder_service_creation(&target.context).await?,
        };
        if let Some(receipt) = &receipt {
            receipt.validate()?;
            if receipt.target.context != target.context
                || !matches!(&receipt.lease,UserAppOperationLeaseReceipt::Kubernetes {namespace,service_type,..} if namespace==&self.namespace && *service_type==ServiceType::UserappBuilder)
            {
                return Err(Error::Conflict(
                    "Original absent builder completion scope differs".into(),
                ));
            }
        }
        Ok(receipt)
    }

    /// Physical write completion, independent of present readiness. This is
    /// deliberately not an RV fence and never releases the coordinator lease.
    pub(crate) async fn absent_builder_creation_acknowledged(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<bool> {
        target.validate().map_err(Error::ConfigurationError)?;
        if target.workload.is_some() {
            return Ok(false);
        }
        let Some(receipt) = self.original_absent_creation_receipt(target).await? else {
            return Ok(false);
        };
        let current = self
            .capture_builder_compute_with_binding(&target.context, None, false)
            .await?;
        let Some(workload) = current.workload.as_ref() else {
            return Ok(true);
        };
        if current
            .workload
            .as_ref()
            .map(|resource| (&resource.name, &resource.uid))
            != receipt
                .target
                .workload
                .as_ref()
                .map(|resource| (&resource.name, &resource.uid))
        {
            return Err(Error::Conflict(
                "Another builder owns the original completion scope".into(),
            ));
        }
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let actual = api
            .get(&workload.name)
            .await
            .map_err(|error| api_error("Observe completed builder ownership", error))?;
        target
            .context
            .validate_operation_metadata(actual.metadata.annotations.as_ref().ok_or_else(|| {
                Error::Conflict("Completed builder operation metadata missing".into())
            })?)
            .map_err(Error::Conflict)?;
        if actual.metadata.uid.as_deref() != Some(workload.uid.as_str()) {
            return Err(Error::Conflict(
                "Completed builder changed during ownership observation".into(),
            ));
        }
        Ok(true)
    }

    pub(crate) async fn reconcile_absent_builder_creation(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Option<ContainerBasicInfo>> {
        target.validate().map_err(Error::ConfigurationError)?;
        let Some(receipt) = self.original_absent_creation_receipt(target).await? else {
            return Ok(None);
        };
        let actual = self
            .capture_builder_compute_with_binding(&target.context, None, false)
            .await?;
        if actual
            .workload
            .as_ref()
            .map(|resource| (&resource.name, &resource.uid))
            != receipt
                .target
                .workload
                .as_ref()
                .map(|resource| (&resource.name, &resource.uid))
            || actual.pod.as_ref().map(|pod| (&pod.name, &pod.uid))
                != receipt.target.pod.as_ref().map(|pod| (&pod.name, &pod.uid))
        {
            return Ok(None);
        }
        let Some(workload) = actual.workload.as_ref() else {
            return Ok(None);
        };
        let controllers: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let controller = controllers
            .get(&workload.name)
            .await
            .map_err(|error| api_error("Verify original absent builder completion", error))?;
        target
            .context
            .validate_operation_metadata(controller.metadata.annotations.as_ref().ok_or_else(
                || Error::Conflict("Created builder execution metadata missing".into()),
            )?)
            .map_err(Error::Conflict)?;
        if controller.metadata.uid.as_deref() != Some(workload.uid.as_str())
            || controller.metadata.deletion_timestamp.is_some()
            || controller.spec.as_ref().and_then(|spec| spec.replicas) != Some(1)
            || !statefulset_agent_image_matches(&controller, target.restart_image.as_deref())
            || !statefulset_agent_workspace_matches(
                &controller,
                Some(&shared_types::paths::userapp_dev_workspace(
                    &target.context.app_id,
                )),
            )
        {
            return Ok(None);
        }
        let Some(expected) = actual.pod.as_ref() else {
            return Ok(None);
        };
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod = pods
            .get(&expected.name)
            .await
            .map_err(|error| api_error("Verify completed builder readiness", error))?;
        if !builder_agent_running(&pod)
            || pod.metadata.deletion_timestamp.is_some()
            || !pod_agent_image_matches(&pod, target.restart_image.as_deref())
            || !pod_agent_workspace_matches(
                &pod,
                Some(&shared_types::paths::userapp_dev_workspace(
                    &target.context.app_id,
                )),
            )
        {
            return Ok(None);
        }
        self.capture_builder_volume_witness(&actual).await?;
        let repeated = self
            .capture_builder_compute_with_binding(&target.context, None, false)
            .await?;
        if repeated
            .workload
            .as_ref()
            .map(|resource| (&resource.name, &resource.uid))
            != actual
                .workload
                .as_ref()
                .map(|resource| (&resource.name, &resource.uid))
            || repeated.pod != actual.pod
        {
            return Err(Error::Conflict(
                "Original absent builder completion changed during observation".into(),
            ));
        }
        Ok(Some(receipt.registration_container()?))
    }

    /// Confirm the original complete multi-step result without requiring a live
    /// writer. Missing/partial receipts never authorize a factory continuation.
    pub(crate) async fn observe_original_builder_creation(
        &self,
        target: &BuilderControlTarget,
        original_lease: &UserAppOperationLeaseReceipt,
    ) -> Result<Option<ContainerBasicInfo>> {
        target.validate().map_err(Error::ConfigurationError)?;
        if target.workload.is_some() || target.pod.is_some() || target.resource_binding.is_some() {
            return Ok(None);
        }
        let Some(completion) = self.original_absent_creation_receipt(target).await? else {
            return Ok(None);
        };
        let same_lease = matches!((&completion.lease, original_lease),
            (UserAppOperationLeaseReceipt::Kubernetes { service_type: a_family, namespace: a_ns, name: a_name, uid: a_uid, token: a_token, .. },
             UserAppOperationLeaseReceipt::Kubernetes { service_type: b_family, namespace: b_ns, name: b_name, uid: b_uid, token: b_token, .. })
            if a_family == b_family && a_ns == b_ns && a_name == b_name && a_uid == b_uid && a_token == b_token);
        if !same_lease {
            return Err(Error::Conflict(
                "Completed builder creation original lease differs".into(),
            ));
        }
        let Some(info) = self.reconcile_absent_builder_creation(target).await? else {
            return Ok(None);
        };
        let actual = self
            .capture_builder_compute_with_binding(&target.context, None, false)
            .await?;
        self.verify_partial_builder_creation(target, &actual)
            .await?;
        let expected_pod = actual
            .pod
            .as_ref()
            .ok_or_else(|| Error::Conflict("Completed builder Pod is missing".into()))?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod = pods
            .get(&expected_pod.name)
            .await
            .map_err(|error| api_error("Observe completed builder Ready status", error))?;
        if pod.metadata.uid.as_deref() != Some(info.container_id.as_str())
            || !pod
                .status
                .as_ref()
                .and_then(|status| status.conditions.as_ref())
                .is_some_and(|conditions| {
                    conditions
                        .iter()
                        .any(|condition| condition.type_ == "Ready" && condition.status == "True")
                })
        {
            return Ok(None);
        }
        let Some(repeated) = self.original_absent_creation_receipt(target).await? else {
            return Err(Error::Conflict(
                "Original builder completion disappeared".into(),
            ));
        };
        if serde_json::to_value(&repeated)
            .map_err(|error| Error::ConfigurationError(error.to_string()))?
            != serde_json::to_value(&completion)
                .map_err(|error| Error::ConfigurationError(error.to_string()))?
        {
            return Err(Error::Conflict(
                "Original builder completion changed during observation".into(),
            ));
        }
        Ok(Some(info))
    }

    async fn verify_partial_builder_creation(
        &self,
        target: &BuilderControlTarget,
        captured: &BuilderControlTarget,
    ) -> Result<()> {
        let workload = captured
            .workload
            .as_ref()
            .ok_or_else(|| Error::Conflict("Original partial builder workload missing".into()))?;
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let current = api
            .get(&workload.name)
            .await
            .map_err(|error| api_error("Inspect original partial builder creation", error))?;
        target
            .context
            .validate_operation_metadata(current.metadata.annotations.as_ref().ok_or_else(
                || Error::Conflict("Partial builder creation metadata missing".into()),
            )?)
            .map_err(Error::Conflict)?;
        if current.metadata.uid.as_deref() != Some(workload.uid.as_str())
            || current.metadata.deletion_timestamp.is_some()
            || !statefulset_agent_image_matches(&current, target.restart_image.as_deref())
            || !statefulset_agent_workspace_matches(
                &current,
                Some(&shared_types::paths::userapp_dev_workspace(
                    &target.context.app_id,
                )),
            )
        {
            return Err(Error::Conflict(
                "Original partial builder physical or startup intent changed".into(),
            ));
        }
        let env = current
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .and_then(|spec| {
                spec.containers
                    .iter()
                    .find(|container| container.name == "agent")
            })
            .and_then(|container| container.env.as_ref());
        let control: Vec<_> = env
            .into_iter()
            .flatten()
            .filter(|entry| entry.name == shared_types::APP_CLI_CONTROL_ONLY_ENV)
            .collect();
        let boot: Vec<_> = env
            .into_iter()
            .flatten()
            .filter(|entry| entry.name == shared_types::APP_CLI_MANAGEMENT_BOOT_ID_ENV)
            .collect();
        let matches_mode =
            if target.builder_startup_mode == shared_types::BuilderStartupMode::ManagementOnly {
                control.len() == 1
                    && control[0].value.as_deref() == Some("true")
                    && boot.len() == 1
                    && boot[0].value.as_deref() == Some(target.context.operation_id.as_str())
            } else {
                control.is_empty() && boot.is_empty()
            };
        if !matches_mode {
            return Err(Error::Conflict(
                "Original partial builder startup mode changed".into(),
            ));
        }
        Ok(())
    }

    /// Absence or unknown replies never authorize another CREATE. A recorded
    /// same-operation partial controller, its exact active compute holder,
    /// canonical startup intent and current PVC permit idempotent completion.
    pub(crate) async fn prepare_absent_builder_retry(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<Option<BuilderControlTarget>> {
        target.validate().map_err(Error::ConfigurationError)?;
        if target.workload.is_some() {
            return Err(Error::Conflict(
                "Partial missing retry has a physical original target".into(),
            ));
        }
        let captured = self
            .capture_builder_compute_with_binding(&target.context, None, false)
            .await?;
        if captured.workload.is_none() {
            return Ok(None);
        }
        self.verify_partial_builder_creation(target, &captured)
            .await?;
        let shared_types::ComputeLeaseInspection::Discovered { receipt, attempt } = self
            .inspect_compute_k8s_lease(
                &target.context,
                shared_types::UserAppOperationScope::Dev,
                None,
            )
            .await?
        else {
            return Err(Error::Conflict(
                "Original partial builder has no exact compute holder".into(),
            ));
        };
        if attempt != target.context
            || !self
                .validate_captured_application_operation(&target.context, &receipt)
                .await?
        {
            return Err(Error::Conflict(
                "Original partial builder compute holder changed".into(),
            ));
        }
        let volumes = self.capture_builder_volume_witness(&captured).await?;
        let repeated = self
            .capture_builder_compute_with_binding(&target.context, None, false)
            .await?;
        if repeated
            .workload
            .as_ref()
            .map(|resource| (&resource.name, &resource.uid))
            != captured
                .workload
                .as_ref()
                .map(|resource| (&resource.name, &resource.uid))
            || self.capture_builder_volume_witness(&repeated).await? != volumes
            || !self
                .validate_captured_application_operation(&target.context, &receipt)
                .await?
        {
            return Err(Error::Conflict(
                "Original partial builder changed before retry confirmation".into(),
            ));
        }
        self.verify_partial_builder_creation(target, &repeated)
            .await?;
        Ok(Some(target.clone()))
    }

    pub(crate) async fn ensure_absent_builder_compute(
        &self,
        target: &BuilderControlTarget,
        volumes: &[AppResourceIdentity],
        receipt: &UserAppOperationLeaseReceipt,
    ) -> Result<Option<ContainerBasicInfo>> {
        target.validate().map_err(Error::ConfigurationError)?;
        if target.workload.is_some()
            || target.pod.is_some()
            || target.resource_binding.is_some()
            || volumes.is_empty()
        {
            return Err(Error::Conflict(
                "Missing builder creation requires an absent target and retained workspace witness"
                    .into(),
            ));
        }
        if receipt.service_type() != &ServiceType::UserappBuilder
            || !self
                .validate_captured_application_operation(&target.context, receipt)
                .await?
        {
            return Err(Error::Conflict(
                "Original builder creation lease is not held".into(),
            ));
        }
        if let Some(completion) = self.original_absent_creation_receipt(target).await? {
            if completion.lease != *receipt {
                return Err(Error::Conflict(
                    "Original builder creation receipt lease differs".into(),
                ));
            }
            let ready = self.reconcile_absent_builder_creation(target).await?;
            if ready.is_some() {
                let actual = self
                    .capture_builder_compute_with_binding(&target.context, None, false)
                    .await?;
                if self.capture_builder_volume_witness(&actual).await? != volumes {
                    return Err(Error::Conflict(
                        "Created builder workspace differs from original volume witness".into(),
                    ));
                }
            }
            return Ok(ready);
        }
        let before = self
            .capture_builder_compute_with_binding(&target.context, None, false)
            .await?;
        if before.workload.is_some() {
            self.verify_partial_builder_creation(target, &before)
                .await?;
            if self.capture_builder_volume_witness(&before).await? != volumes {
                return Err(Error::Conflict(
                    "Partial original builder workspace changed".into(),
                ));
            }
        } else if self.capture_absent_builder_volumes(target).await? != volumes {
            return Err(Error::Conflict(
                "Retained builder workspace differs before creation".into(),
            ));
        }
        if !self
            .validate_captured_application_operation(&target.context, receipt)
            .await?
        {
            return Err(Error::Conflict(
                "Builder creation lease changed before dispatch".into(),
            ));
        }
        let mut params = ContainerCreateParams::builder()
            .project_id(target.context.app_id.clone())
            .builder_app_id(target.context.app_id.clone())
            .service_type(ServiceType::UserappBuilder)
            .execution_context(target.context.clone())
            .builder_startup_mode(target.builder_startup_mode)
            .build();
        params.image_override = target.restart_image.clone();
        let info = self
            .create_agent_container_with_creation_receipt(params, Some(receipt.clone()))
            .await?;
        let actual = self
            .capture_builder_compute_with_binding(&target.context, None, false)
            .await?;
        if before.workload.is_some()
            && actual
                .workload
                .as_ref()
                .map(|resource| (&resource.name, &resource.uid))
                != before
                    .workload
                    .as_ref()
                    .map(|resource| (&resource.name, &resource.uid))
        {
            return Err(Error::Conflict(
                "Original partial builder physical identity changed during continuation".into(),
            ));
        }
        if self.capture_builder_volume_witness(&actual).await? != volumes
            || !self
                .validate_captured_application_operation(&target.context, receipt)
                .await?
        {
            return Err(Error::Conflict(
                "Created builder volume or original lease changed before completion".into(),
            ));
        }
        let completed = super::super::builder_creation_receipt::BuilderCreationReceipt {
            target: actual,
            container: info,
            lease: receipt.clone(),
        };
        self.save_builder_creation_receipt(&completed).await?;
        self.reconcile_absent_builder_creation(target).await
    }
}
