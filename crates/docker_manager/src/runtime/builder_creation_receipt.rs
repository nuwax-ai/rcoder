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
