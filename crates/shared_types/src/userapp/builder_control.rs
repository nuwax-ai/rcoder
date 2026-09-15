//! Compute-only builder controls. Storage receipts are deliberately absent so a
//! stop/restart cannot accidentally delegate to full builder/PVC deletion.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderPodIdentity {
    pub name: String,
    pub uid: String,
    pub resource_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderControlTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_binding: Option<crate::UserAppResourceBinding>,
    pub context: crate::UserAppExecutionContext,
    pub workload: Option<crate::AppResourceIdentity>,
    pub pod: Option<BuilderPodIdentity>,
}

impl BuilderControlTarget {
    pub fn validate(&self) -> Result<(), String> {
        self.context.validate_identity(&self.context.app_id)?;
        if let Some(workload) = &self.workload {
            if workload.name.is_empty() || workload.uid.is_empty() {
                return Err("Builder compute identity is missing".into());
            }
            if let Some(binding) = &self.resource_binding {
                binding.validate(&self.context, &workload.uid)?;
            }
            match workload.kind {
                crate::AppResourceKind::Container if self.pod.is_none() => {}
                crate::AppResourceKind::StatefulSet
                    if workload
                        .resource_version
                        .as_ref()
                        .is_some_and(|version| !version.is_empty()) => {}
                _ => return Err("Builder control requires a compute-only resource identity".into()),
            }
        } else if self.pod.is_some() || self.resource_binding.is_some() {
            return Err("Builder pod has no captured workload".into());
        }
        if self.pod.as_ref().is_some_and(|pod| {
            pod.name.is_empty() || pod.uid.is_empty() || pod.resource_version.is_empty()
        }) {
            return Err("Builder pod identity is incomplete".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuilderControlResult {
    pub operation_id: String,
    pub was_existing: bool,
    pub container: Option<crate::ContainerBasicInfo>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct BuilderControlError {
    pub operation_id: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_control_never_accepts_storage_or_incomplete_pod_receipts() {
        let context = crate::UserAppExecutionContext {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            operation_id: "stop".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let mut target = BuilderControlTarget {
            resource_binding: None,
            context,
            workload: None,
            pod: None,
        };
        target.validate().expect("authoritative compute absence");
        for kind in [
            crate::AppResourceKind::PersistentVolumeClaim,
            crate::AppResourceKind::Service,
            crate::AppResourceKind::Deployment,
        ] {
            target.workload = Some(crate::AppResourceIdentity {
                kind,
                name: "resource".into(),
                uid: "uid".into(),
                resource_version: Some("1".into()),
            });
            assert!(
                target.validate().is_err(),
                "non-builder resource accepted: {kind:?}"
            );
        }
        target.workload = Some(crate::AppResourceIdentity {
            kind: crate::AppResourceKind::StatefulSet,
            name: "builder".into(),
            uid: "sts-uid".into(),
            resource_version: Some("1".into()),
        });
        target.pod = Some(BuilderPodIdentity {
            name: "builder-0".into(),
            uid: "pod-uid".into(),
            resource_version: "2".into(),
        });
        target.validate().expect("complete compute target");
        target.pod.as_mut().expect("pod").resource_version.clear();
        assert!(target.validate().is_err());
        target.pod = None;
        target.workload.as_mut().expect("workload").resource_version = None;
        assert!(target.validate().is_err());
    }
}

/// Written only after create returned successfully (including mutex release) and
/// readiness plus physical ownership were independently confirmed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderCreationEvidence {
    pub creation_lease_released: bool,
    pub target: BuilderControlTarget,
    pub container: crate::ContainerBasicInfo,
}
impl BuilderCreationEvidence {
    pub fn validate_operation(
        &self,
        operation: &crate::UserAppOperationRecord,
    ) -> Result<(), String> {
        self.target.validate()?;
        let context = &self.target.context;
        if !self.creation_lease_released
            || operation.kind != crate::UserAppOperationKind::EnsureBuilder
            || context.app_id != operation.app_id
            || context.lifecycle_id != operation.lifecycle_id
            || context.operation_id != operation.operation_id
            || operation.executor_id.as_deref() != Some(context.executor_id.as_str())
            || context.request_fingerprint != operation.request_fingerprint
        {
            return Err("Builder completion operation identity mismatch".into());
        }
        let workload = self
            .target
            .workload
            .as_ref()
            .ok_or("Builder completion workload missing")?;
        if workload.kind == crate::AppResourceKind::StatefulSet && self.target.pod.is_none() {
            return Err("Builder completion pod identity missing".into());
        }
        let physical = self
            .target
            .pod
            .as_ref()
            .map_or(workload.uid.as_str(), |pod| pod.uid.as_str());
        if self.container.container_id.is_empty() || physical != self.container.container_id {
            return Err("Builder completion physical identity mismatch".into());
        }
        Ok(())
    }
}
