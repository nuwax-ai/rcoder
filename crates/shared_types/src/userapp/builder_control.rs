//! Compute-only builder controls. Storage receipts are deliberately absent so a
//! stop/restart cannot accidentally delegate to full builder/PVC deletion.
use serde::{Deserialize, Serialize};

/// Public checkpoint references private runtime configuration; it never embeds
/// container environment variables or credentials in an operation response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderRestartTemplate {
    pub source: BuilderControlTarget,
    pub archive: crate::AppResourceIdentity,
    pub volumes: Vec<crate::AppResourceIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderPodIdentity {
    pub name: String,
    pub uid: String,
    pub resource_version: String,
}

/// Compute-only deletion of a Pod whose original StatefulSet no longer exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderOrphanStopTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_binding: Option<crate::UserAppResourceBinding>,
    pub context: crate::UserAppExecutionContext,
    pub orphan_pod: BuilderPodIdentity,
    pub controller_name: String,
    pub controller_uid: String,
}
impl BuilderOrphanStopTarget {
    pub fn validate(&self) -> Result<(), String> {
        self.context.validate_identity(&self.context.app_id)?;
        if let Some(binding) = &self.resource_binding {
            binding.validate(&self.context, &self.controller_uid)?;
        }
        if self.controller_name.is_empty()
            || self.controller_uid.is_empty()
            || self.orphan_pod.name.is_empty()
            || self.orphan_pod.uid.is_empty()
            || self.orphan_pod.resource_version.is_empty()
        {
            return Err("Orphan builder physical identity missing".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuilderControlTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_binding: Option<crate::UserAppResourceBinding>,
    pub context: crate::UserAppExecutionContext,
    pub workload: Option<crate::AppResourceIdentity>,
    pub pod: Option<BuilderPodIdentity>,
    /// Image selected for this compute start. Persisted with the operation so
    /// recovery never picks a different platform release after a rollout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_image: Option<String>,
    /// Platform-owned source directory frozen before a Kubernetes dev restart.
    /// Missing in old checkpoints; recovery must never invent a new intent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_runtime_workspace: Option<String>,
}

// The compute ledger adds protocol and volume witnesses alongside the target.
// Accept those explicit fields without weakening rejection of unrelated input.
impl<'de> Deserialize<'de> for BuilderControlTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(default)]
            resource_binding: Option<crate::UserAppResourceBinding>,
            context: crate::UserAppExecutionContext,
            workload: Option<crate::AppResourceIdentity>,
            pod: Option<BuilderPodIdentity>,
            #[serde(default)]
            restart_image: Option<String>,
            #[serde(default)]
            restart_runtime_workspace: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            builder_compute_single_write: bool,
            #[serde(default)]
            #[allow(dead_code)]
            builder_volumes: Vec<crate::AppResourceIdentity>,
            #[serde(default)]
            #[allow(dead_code)]
            builder_restart_template: Option<BuilderRestartTemplate>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            resource_binding: wire.resource_binding,
            context: wire.context,
            workload: wire.workload,
            pod: wire.pod,
            restart_image: wire.restart_image,
            restart_runtime_workspace: wire.restart_runtime_workspace,
        })
    }
}

impl BuilderControlTarget {
    pub fn validate(&self) -> Result<(), String> {
        self.context.validate_identity(&self.context.app_id)?;
        if self
            .restart_image
            .as_deref()
            .is_some_and(|image| image.trim().is_empty())
        {
            return Err("Builder restart image is empty".into());
        }
        if let Some(root) = self.restart_runtime_workspace.as_deref()
            && (root != crate::paths::userapp_dev_workspace(&self.context.app_id)
                || self
                    .workload
                    .as_ref()
                    .is_none_or(|workload| workload.kind != crate::AppResourceKind::StatefulSet))
        {
            return Err(
                "Builder restart workspace requires the captured dev StatefulSet source root"
                    .into(),
            );
        }
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
    fn restart_workspace_wire_is_additive_and_rejects_foreign_root() {
        let legacy = serde_json::json!({
            "resource_binding":null,
            "context":{"app_id":"228","lifecycle_id":"life","operation_id":"restart","executor_id":"worker","request_fingerprint":"a".repeat(64)},
            "workload":{"kind":"stateful_set","name":"builder","uid":"sts","resource_version":"1"},
            "pod":null
        });
        // Serialize the identity using its enum representation rather than
        // coupling the fixture to the storage spelling of resource kinds.
        let mut legacy = legacy;
        legacy["workload"] = serde_json::to_value(crate::AppResourceIdentity {
            kind: crate::AppResourceKind::StatefulSet,
            name: "builder".into(),
            uid: "sts".into(),
            resource_version: Some("1".into()),
        })
        .unwrap();
        let mut target: BuilderControlTarget = serde_json::from_value(legacy).unwrap();
        assert_eq!(target.restart_runtime_workspace, None);
        assert!(
            serde_json::to_value(&target)
                .unwrap()
                .get("restart_runtime_workspace")
                .is_none()
        );
        target.restart_runtime_workspace = Some("/home/user/228".into());
        target.validate().unwrap();
        let value = serde_json::to_value(&target).unwrap();
        assert_eq!(
            serde_json::from_value::<BuilderControlTarget>(value).unwrap(),
            target
        );
        target.restart_runtime_workspace = Some("/home/user/other".into());
        assert!(target.validate().is_err());
        target.restart_runtime_workspace = Some("/home/user/228".into());
        target.workload.as_mut().unwrap().kind = crate::AppResourceKind::Container;
        assert!(target.validate().is_err());
    }

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
            restart_image: None,
            restart_runtime_workspace: None,
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

/// Written after create returned successfully (including mutex release) and
/// physical ownership was confirmed. The operation step distinguishes
/// builder_created_observed from builder_ready_confirmed; only the latter
/// includes independently verified management readiness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderCreationPredecessor {
    pub target: BuilderControlTarget,
    pub volumes: Vec<crate::AppResourceIdentity>,
}

impl BuilderCreationPredecessor {
    pub fn validate_operation(
        &self,
        operation: &crate::UserAppOperationRecord,
    ) -> Result<(), String> {
        self.target.validate()?;
        let context = &self.target.context;
        if operation.kind != crate::UserAppOperationKind::EnsureBuilder
            || context.app_id != operation.app_id
            || context.lifecycle_id != operation.lifecycle_id
            || context.operation_id != operation.operation_id
            || operation.executor_id.as_deref() != Some(context.executor_id.as_str())
            || context.request_fingerprint != operation.request_fingerprint
            || self
                .target
                .workload
                .as_ref()
                .is_none_or(|resource| resource.kind != crate::AppResourceKind::StatefulSet)
            || self.volumes.is_empty()
            || self.volumes.iter().any(|volume| {
                volume.kind != crate::AppResourceKind::PersistentVolumeClaim
                    || volume.name.is_empty()
                    || volume.uid.is_empty()
            })
        {
            return Err("Builder predecessor operation or volume identity differs".into());
        }
        Ok(())
    }

    pub fn validate_replacement(
        &self,
        target: &BuilderControlTarget,
        volumes: &[crate::AppResourceIdentity],
    ) -> Result<(), String> {
        self.target.validate()?;
        target.validate()?;
        let source = self
            .target
            .workload
            .as_ref()
            .ok_or("Builder predecessor workload missing")?;
        let replacement = target
            .workload
            .as_ref()
            .ok_or("Builder replacement workload missing")?;
        if self.target.context != target.context
            || source.kind != crate::AppResourceKind::StatefulSet
            || replacement.kind != crate::AppResourceKind::StatefulSet
            || source.name != replacement.name
            || source.uid == replacement.uid
        {
            return Err("Builder replacement operation or workload identity differs".into());
        }
        let identities = |items: &[crate::AppResourceIdentity]| -> Result<std::collections::BTreeMap<String, String>, String> {
            let mut result = std::collections::BTreeMap::new();
            for item in items {
                if item.kind != crate::AppResourceKind::PersistentVolumeClaim
                    || item.name.is_empty() || item.uid.is_empty()
                    || result.insert(item.name.clone(), item.uid.clone()).is_some()
                {
                    return Err("Builder replacement volume identity is incomplete".into());
                }
            }
            if result.is_empty() {
                return Err("Builder replacement requires captured workspace volumes".into());
            }
            Ok(result)
        };
        if identities(&self.volumes)? != identities(volumes)? {
            return Err("Builder replacement workspace volume identity changed".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuilderCreationEvidence {
    pub creation_lease_released: bool,
    pub target: BuilderControlTarget,
    pub container: crate::ContainerBasicInfo,
    /// Original workload and PVC identities captured before this operation's
    /// controlled replacement. Omitted for ordinary creation/reuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_predecessor: Option<BuilderCreationPredecessor>,
}
impl BuilderCreationEvidence {
    /// Registration-only confirmation of a succeeded EnsureBuilder. A private
    /// completion receipt can acknowledge a resource created by an earlier
    /// operation; it never proves historical volume preservation or authorizes
    /// another runtime write. Storage verifies any existing canonical binding.
    pub fn validate_registration_replacement(
        &self,
        operation: &crate::UserAppOperationRecord,
        volumes: &[crate::AppResourceIdentity],
    ) -> Result<(), String> {
        self.validate_operation(operation)?;
        if operation.state != crate::UserAppOperationState::Succeeded {
            return Err("Builder registration requires a succeeded EnsureBuilder operation".into());
        }
        let workload = self
            .target
            .workload
            .as_ref()
            .ok_or("Builder completion workload missing")?;
        if let Some(binding) = &self.target.resource_binding {
            binding.validate(&self.target.context, &workload.uid)?;
            if binding.service_type != crate::ServiceType::UserappBuilder {
                return Err("Builder completion binding has the wrong resource family".into());
            }
        }
        if let Some(source) = &self.registration_predecessor {
            return source.validate_replacement(&self.target, volumes);
        }
        if workload.kind != crate::AppResourceKind::StatefulSet
            || self
                .container
                .workload_uid
                .as_deref()
                .is_some_and(|uid| uid != workload.uid)
            || volumes.is_empty()
            || volumes.iter().any(|volume| {
                volume.kind != crate::AppResourceKind::PersistentVolumeClaim
                    || volume.name.is_empty()
                    || volume.uid.is_empty()
            })
            || volumes
                .iter()
                .map(|volume| &volume.name)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != volumes.len()
        {
            return Err(
                "Builder completion receipt or current volume identity is incomplete".into(),
            );
        }
        Ok(())
    }

    pub fn validate_operation(
        &self,
        operation: &crate::UserAppOperationRecord,
    ) -> Result<(), String> {
        self.target.validate()?;
        let context = &self.target.context;
        if !self.creation_lease_released
            || operation.kind != crate::UserAppOperationKind::EnsureBuilder
            || operation.scope != crate::UserAppOperationScope::Dev
            || self
                .target
                .resource_binding
                .as_ref()
                .is_some_and(|binding| binding.service_type != crate::ServiceType::UserappBuilder)
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
        if workload.kind == crate::AppResourceKind::StatefulSet
            && (self.target.pod.is_none()
                || self
                    .container
                    .workload_uid
                    .as_deref()
                    .is_some_and(|uid| uid != workload.uid))
        {
            return Err("Builder completion pod or workload identity missing".into());
        }
        let physical = self
            .target
            .pod
            .as_ref()
            .map_or(workload.uid.as_str(), |pod| pod.uid.as_str());
        if self.container.container_id.is_empty() || physical != self.container.container_id {
            return Err("Builder completion physical identity mismatch".into());
        }
        if let Some(predecessor) = &self.registration_predecessor {
            predecessor.validate_operation(operation)?;
            if predecessor
                .target
                .workload
                .as_ref()
                .is_some_and(|source| source.uid == workload.uid)
            {
                return Err("Builder replacement did not change workload identity".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod registration_confirmation_tests {
    use super::*;
    use crate::*;

    fn completed() -> (
        UserAppOperationRecord,
        BuilderCreationEvidence,
        Vec<AppResourceIdentity>,
    ) {
        let context = UserAppExecutionContext {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            operation_id: "confirmation".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let container = ContainerBasicInfo {
            container_id: "pod".into(),
            container_name: "builder".into(),
            container_ip: "10.0.0.8".into(),
            internal_port: 60000,
            external_port: 0,
            project_id: "app".into(),
            status: "Running".into(),
            created_at: chrono::Utc::now(),
            service_url: "http://10.0.0.8:60000".into(),
            workload_uid: Some("sts".into()),
        };
        let operation = UserAppOperationRecord {
            runtime_policy_on_success: None,
            command: None,
            admitted_metadata: None,
            operation_id: context.operation_id.clone(),
            app_id: context.app_id.clone(),
            lifecycle_id: context.lifecycle_id.clone(),
            request_id: None,
            request_fingerprint: context.request_fingerprint.clone(),
            kind: UserAppOperationKind::EnsureBuilder,
            scope: UserAppOperationScope::Dev,
            state: UserAppOperationState::Succeeded,
            revision: 3,
            executor_id: Some("worker".into()),
            step: "builder_ready_confirmed".into(),
            checkpoint: serde_json::to_value(&container).unwrap(),
            error_code: None,
            error_message: None,
            created_at: container.created_at,
        };
        let evidence = BuilderCreationEvidence {
            creation_lease_released: true,
            container,
            registration_predecessor: None,
            target: BuilderControlTarget {
                resource_binding: None,
                context,
                workload: Some(AppResourceIdentity {
                    kind: AppResourceKind::StatefulSet,
                    name: "builder".into(),
                    uid: "sts".into(),
                    resource_version: Some("1".into()),
                }),
                pod: Some(BuilderPodIdentity {
                    name: "builder-0".into(),
                    uid: "pod".into(),
                    resource_version: "1".into(),
                }),
                restart_image: None,
                restart_runtime_workspace: None,
            },
        };
        let volumes = vec![AppResourceIdentity {
            kind: AppResourceKind::PersistentVolumeClaim,
            name: "workspace".into(),
            uid: "pvc".into(),
            resource_version: None,
        }];
        (operation, evidence, volumes)
    }

    #[test]
    fn succeeded_confirmation_with_null_binding_and_legacy_uid_is_not_historical_creation() {
        let (operation, mut evidence, volumes) = completed();
        evidence
            .validate_registration_replacement(&operation, &volumes)
            .unwrap();
        assert!(evidence.registration_predecessor.is_none());
        // The mandatory target already pins the STS and Pod. Old responses
        // omitted only this redundant container field; contradictory Some does not pass.
        evidence.container.workload_uid = None;
        evidence
            .validate_registration_replacement(&operation, &volumes)
            .unwrap();
        evidence.container.workload_uid = Some("different-sts".into());
        assert!(
            evidence
                .validate_registration_replacement(&operation, &volumes)
                .is_err()
        );
    }

    #[test]
    fn existing_canonical_binding_can_belong_to_an_earlier_confirmation_but_never_another_resource()
    {
        let (operation, mut evidence, volumes) = completed();
        let binding = UserAppResourceBinding {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            service_type: ServiceType::UserappBuilder,
            physical_uid: "sts".into(),
            adopted_by_operation: "earlier-confirmation".into(),
        };
        evidence.target.resource_binding = Some(binding.clone());
        evidence
            .validate_registration_replacement(&operation, &volumes)
            .unwrap();
        for field in ["application", "lifecycle", "physical", "family", "adoption"] {
            let mut invalid = binding.clone();
            match field {
                "application" => invalid.app_id = "foreign".into(),
                "lifecycle" => invalid.lifecycle_id = "old".into(),
                "physical" => invalid.physical_uid = "old-sts".into(),
                "family" => invalid.service_type = ServiceType::Userapp,
                _ => invalid.adopted_by_operation.clear(),
            }
            evidence.target.resource_binding = Some(invalid);
            assert!(
                evidence
                    .validate_registration_replacement(&operation, &volumes)
                    .is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn registration_requires_succeeded_exact_dev_ensure_and_unique_nonempty_pvcs() {
        let (operation, evidence, volumes) = completed();
        for field in ["state", "kind", "scope", "executor", "fingerprint"] {
            let mut invalid = operation.clone();
            match field {
                "state" => invalid.state = UserAppOperationState::Failed,
                "kind" => invalid.kind = UserAppOperationKind::StopBuilder,
                "scope" => invalid.scope = UserAppOperationScope::Prod,
                "executor" => invalid.executor_id = Some("foreign".into()),
                _ => invalid.request_fingerprint = "b".repeat(64),
            }
            assert!(
                evidence
                    .validate_registration_replacement(&invalid, &volumes)
                    .is_err(),
                "{field}"
            );
        }
        assert!(
            evidence
                .validate_registration_replacement(&operation, &[])
                .is_err()
        );
        assert!(
            evidence
                .validate_registration_replacement(
                    &operation,
                    &[volumes[0].clone(), volumes[0].clone()]
                )
                .is_err()
        );
        let mut wrong = volumes.clone();
        wrong[0].uid.clear();
        assert!(
            evidence
                .validate_registration_replacement(&operation, &wrong)
                .is_err()
        );
    }
}
