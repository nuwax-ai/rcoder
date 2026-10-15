//! Acknowledged creation and original lease, saved before lease release.
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use shared_types::{
    BuilderControlTarget, ContainerBasicInfo, ServiceType, UserAppOperationLeaseReceipt,
};

/// Written after the worker has stopped issuing mutations, before releasing
/// its original lease. This is not evidence that no resource was created.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BuilderCancellationReceipt {
    pub context: shared_types::UserAppExecutionContext,
    pub lease: UserAppOperationLeaseReceipt,
}
impl BuilderCancellationReceipt {
    pub(super) fn validate(&self) -> Result<()> {
        self.context
            .validate_identity(&self.context.app_id)
            .map_err(Error::ConfigurationError)?;
        self.lease.validate().map_err(Error::ConfigurationError)?;
        if self.lease.service_type() != &ServiceType::UserappBuilder {
            return Err(Error::Conflict(
                "Builder cancellation lease family differs".into(),
            ));
        }
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BuilderCreationReceipt {
    pub target: BuilderControlTarget,
    pub container: ContainerBasicInfo,
    pub lease: UserAppOperationLeaseReceipt,
}
impl BuilderCreationReceipt {
    /// Older Kubernetes acknowledgements omitted the controller UID from the
    /// returned container DTO. The receipt already captures it independently
    /// in its workload target; recover only this missing redundant field.
    /// A conflicting value or another logical workload name is never repaired.
    #[cfg(any(feature = "kubernetes", test))]
    pub(super) fn registration_container(&self) -> Result<ContainerBasicInfo> {
        self.validate()?;
        let mut container = self.container.clone();
        let Some(workload) = self.target.workload.as_ref() else {
            return Err(Error::Conflict("Builder creation workload missing".into()));
        };
        if workload.kind == shared_types::AppResourceKind::StatefulSet {
            if container.project_id != self.target.context.app_id
                || container.container_name != workload.name
                || container
                    .workload_uid
                    .as_ref()
                    .is_some_and(|uid| uid != &workload.uid)
            {
                return Err(Error::Conflict(
                    "Builder creation container belongs to another workload".into(),
                ));
            }
            container.workload_uid = Some(workload.uid.clone());
        }
        Ok(container)
    }

    pub(super) fn validate(&self) -> Result<()> {
        self.target.validate().map_err(Error::ConfigurationError)?;
        self.lease.validate().map_err(Error::ConfigurationError)?;
        let workload = self
            .target
            .workload
            .as_ref()
            .ok_or_else(|| Error::Conflict("Builder creation workload missing".into()))?;
        let runtime_matches = matches!(
            (&self.lease, workload.kind),
            (
                UserAppOperationLeaseReceipt::Docker { .. },
                shared_types::AppResourceKind::Container
            ) | (
                UserAppOperationLeaseReceipt::Kubernetes { .. },
                shared_types::AppResourceKind::StatefulSet
            )
        );
        let physical = self
            .target
            .pod
            .as_ref()
            .map_or(workload.uid.as_str(), |pod| pod.uid.as_str());
        if !runtime_matches
            || self.lease.service_type() != &ServiceType::UserappBuilder
            || physical != self.container.container_id
            || (workload.kind == shared_types::AppResourceKind::StatefulSet
                && self.target.pod.is_none())
        {
            return Err(Error::Conflict(
                "Builder creation receipt identity differs".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_receipt() -> BuilderCreationReceipt {
        serde_json::from_value(serde_json::json!({
            "target": {
                "context": {"app_id":"31", "lifecycle_id":"life-31", "operation_id":"ensure-31",
                    "executor_id":"worker-31", "request_fingerprint":"a".repeat(64)},
                "workload": {"kind":"StatefulSet", "name":"rcoder-app-builder-31", "uid":"sts-31", "resource_version":"7"},
                "pod": {"name":"rcoder-app-builder-31-0", "uid":"pod-31", "resource_version":"8"}
            },
            "container": {"container_id":"pod-31", "container_name":"rcoder-app-builder-31",
                "container_ip":"10.42.1.131", "internal_port":8086, "external_port":0,
                "project_id":"31", "status":"running", "created_at":"2026-10-09T17:08:11Z",
                "service_url":"http://rcoder-app-builder-31-svc.test.svc.cluster.local:8086"},
            "lease": {"runtime":"kubernetes", "service_type":"user-app-builder", "namespace":"test",
                "name":"rcoder-operation-builder-31", "uid":"lease-31", "resource_version":"9", "token":"ensure-31"}
        }))
        .expect("actual legacy receipt shape")
    }

    #[test]
    fn registration_receipt_recovers_only_missing_redundant_workload_uid() {
        let receipt = legacy_receipt();
        assert!(receipt.target.resource_binding.is_none());
        assert!(receipt.container.workload_uid.is_none());
        let container = receipt
            .registration_container()
            .expect("checked target UID");
        assert_eq!(container.workload_uid.as_deref(), Some("sts-31"));
        assert_eq!(container.container_id, "pod-31");
        assert_eq!(container.container_name, "rcoder-app-builder-31");
        // Reading a legacy acknowledgement does not rewrite its stored payload.
        assert!(receipt.container.workload_uid.is_none());
    }

    #[test]
    fn registration_receipt_rejects_conflicting_present_workload_uid() {
        let mut receipt = legacy_receipt();
        receipt.container.workload_uid = Some("another-sts".into());
        assert!(receipt.registration_container().is_err());
    }

    #[test]
    fn registration_receipt_keeps_logical_name_and_physical_owner_fences() {
        let mut receipt = legacy_receipt();
        receipt.container.container_name = "another-builder".into();
        assert!(receipt.registration_container().is_err());
        let mut receipt = legacy_receipt();
        receipt.container.container_id = "another-pod".into();
        assert!(receipt.registration_container().is_err());
        let mut receipt = legacy_receipt();
        receipt.container.project_id = "another-app".into();
        assert!(receipt.registration_container().is_err());
    }
}
