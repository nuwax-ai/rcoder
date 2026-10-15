//! Read an already submitted stop's physical target without decoding old diagnostics.
//! This projection grants no authority to dispatch or repeat a runtime mutation.
use crate::{
    AppResourceIdentity, BuilderControlTarget, BuilderOrphanStopTarget, BuilderPodIdentity,
    ComputeAbsenceCheckpoint, UserAppExecutionContext, UserAppMutationTarget,
    UserAppOperationScope, UserAppResourceBinding,
};
use serde::Deserialize;

#[derive(Debug)]
pub enum ComputeStopObservation {
    Absent(ComputeAbsenceCheckpoint),
    Orphan(BuilderOrphanStopTarget),
    Builder(BuilderControlTarget),
    Application(UserAppMutationTarget),
}

impl ComputeStopObservation {
    pub fn from_checkpoint(
        checkpoint: &serde_json::Value,
        scope: UserAppOperationScope,
    ) -> Result<Self, serde_json::Error> {
        // Known physical fields keep their types. A second physical shape is
        // contradictory evidence, even when its resources have since vanished.
        #[derive(Deserialize)]
        struct Shape {
            compute_absent: Option<bool>,
            orphan_pod: Option<BuilderPodIdentity>,
            workload: Option<AppResourceIdentity>,
            pod: Option<BuilderPodIdentity>,
            resource: Option<AppResourceIdentity>,
        }
        let shape: Shape = serde_json::from_value(checkpoint.clone())?;
        let absence = shape.compute_absent == Some(true);
        let orphan = shape.orphan_pod.is_some();
        let builder = shape.workload.is_some() || shape.pod.is_some();
        let application = shape.resource.is_some();
        if [absence, orphan, builder, application]
            .into_iter()
            .filter(|present| *present)
            .count()
            != 1
        {
            return Err(invalid(
                "Stop checkpoint has missing or conflicting physical targets",
            ));
        }
        if absence {
            if !matches!(
                scope,
                UserAppOperationScope::Dev | UserAppOperationScope::Prod
            ) {
                return Err(invalid("Stop absence has conflicting scope"));
            }
            #[derive(Deserialize)]
            struct Absence {
                context: UserAppExecutionContext,
                compute_absent: bool,
            }
            let observed: Absence = serde_json::from_value(checkpoint.clone())?;
            return Ok(Self::Absent(ComputeAbsenceCheckpoint {
                context: observed.context,
                compute_absent: observed.compute_absent,
            }));
        }
        if orphan {
            if scope != UserAppOperationScope::Dev {
                return Err(invalid(
                    "Orphan stop requires the original development scope",
                ));
            }
            #[derive(Deserialize)]
            struct Orphan {
                resource_binding: Option<UserAppResourceBinding>,
                context: UserAppExecutionContext,
                orphan_pod: BuilderPodIdentity,
                controller_name: String,
                controller_uid: String,
            }
            let observed: Orphan = serde_json::from_value(checkpoint.clone())?;
            return Ok(Self::Orphan(BuilderOrphanStopTarget {
                resource_binding: observed.resource_binding,
                context: observed.context,
                orphan_pod: observed.orphan_pod,
                controller_name: observed.controller_name,
                controller_uid: observed.controller_uid,
            }));
        }
        if builder && scope == UserAppOperationScope::Dev {
            return BuilderControlTarget::from_observation_checkpoint(checkpoint)
                .map(Self::Builder);
        }
        if application && scope == UserAppOperationScope::Prod {
            return serde_json::from_value(checkpoint.clone()).map(Self::Application);
        }
        Err(invalid(
            "Stop target differs from its original compute scope",
        ))
    }

    pub fn context(&self) -> &UserAppExecutionContext {
        match self {
            Self::Absent(target) => &target.context,
            Self::Orphan(target) => &target.context,
            Self::Builder(target) => &target.context,
            Self::Application(target) => &target.context,
        }
    }
}

fn invalid(message: &str) -> serde_json::Error {
    serde::de::Error::custom(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppResourceKind, BuilderStartupMode};

    fn targets() -> [(serde_json::Value, UserAppOperationScope); 4] {
        let context = UserAppExecutionContext {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            operation_id: "stop".into(),
            executor_id: "executor".into(),
            request_fingerprint: "a".repeat(64),
        };
        let resource = AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: "builder".into(),
            uid: "actual-controller".into(),
            resource_version: Some("1".into()),
        };
        [
            (
                serde_json::to_value(ComputeAbsenceCheckpoint {
                    context: context.clone(),
                    compute_absent: true,
                })
                .unwrap(),
                UserAppOperationScope::Dev,
            ),
            (
                serde_json::to_value(BuilderOrphanStopTarget {
                    context: context.clone(),
                    resource_binding: None,
                    orphan_pod: BuilderPodIdentity {
                        name: "builder-0".into(),
                        uid: "actual-pod".into(),
                        resource_version: "2".into(),
                    },
                    controller_name: resource.name.clone(),
                    controller_uid: resource.uid.clone(),
                })
                .unwrap(),
                UserAppOperationScope::Dev,
            ),
            (
                serde_json::to_value(BuilderControlTarget {
                    context: context.clone(),
                    resource_binding: None,
                    workload: Some(resource.clone()),
                    pod: None,
                    restart_image: None,
                    restart_runtime_workspace: None,
                    builder_startup_mode: BuilderStartupMode::default(),
                })
                .unwrap(),
                UserAppOperationScope::Dev,
            ),
            (
                serde_json::to_value(UserAppMutationTarget {
                    context,
                    resource: AppResourceIdentity {
                        kind: AppResourceKind::Deployment,
                        ..resource
                    },
                })
                .unwrap(),
                UserAppOperationScope::Prod,
            ),
        ]
    }

    #[test]
    fn stop_observation_keeps_current_wire_strict_and_old_diagnostics_opaque() {
        for (index, (mut checkpoint, scope)) in targets().into_iter().enumerate() {
            checkpoint["old_diagnostic"] = serde_json::json!({"owner_epoch": "unknown"});
            let original = checkpoint.clone();
            let observed = ComputeStopObservation::from_checkpoint(&checkpoint, scope).unwrap();
            assert_eq!(observed.context().app_id, "app");
            assert!(matches!(
                (index, observed),
                (0, ComputeStopObservation::Absent(_))
                    | (1, ComputeStopObservation::Orphan(_))
                    | (2, ComputeStopObservation::Builder(_))
                    | (3, ComputeStopObservation::Application(_))
            ));
            assert_eq!(checkpoint, original);
            match index {
                0 => {
                    assert!(serde_json::from_value::<ComputeAbsenceCheckpoint>(checkpoint).is_err())
                }
                1 => {
                    assert!(serde_json::from_value::<BuilderOrphanStopTarget>(checkpoint).is_err())
                }
                2 => assert!(serde_json::from_value::<BuilderControlTarget>(checkpoint).is_err()),
                3 => assert!(serde_json::from_value::<UserAppMutationTarget>(checkpoint).is_ok()),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn stop_observation_rejects_ambiguous_or_wrong_scope_physical_shapes() {
        let targets = targets();
        for (index, (checkpoint, scope)) in targets.iter().enumerate() {
            assert!(
                ComputeStopObservation::from_checkpoint(
                    checkpoint,
                    UserAppOperationScope::Application
                )
                .is_err()
            );
            if index != 0 {
                let wrong_scope = if *scope == UserAppOperationScope::Dev {
                    UserAppOperationScope::Prod
                } else {
                    UserAppOperationScope::Dev
                };
                assert!(ComputeStopObservation::from_checkpoint(checkpoint, wrong_scope).is_err());
            }
            for (other_index, (other, _)) in targets.iter().enumerate() {
                if index == other_index {
                    continue;
                }
                let mut mixed = checkpoint.clone();
                mixed
                    .as_object_mut()
                    .unwrap()
                    .extend(other.as_object().unwrap().clone());
                assert!(
                    ComputeStopObservation::from_checkpoint(&mixed, *scope).is_err(),
                    "{mixed}"
                );
            }
        }
        let (absence, scope) = &targets[0];
        let mut false_absence = absence.clone();
        false_absence["compute_absent"] = false.into();
        assert!(ComputeStopObservation::from_checkpoint(&false_absence, *scope).is_err());
        let mut missing_orphan = targets[1].0.clone();
        missing_orphan.as_object_mut().unwrap().remove("orphan_pod");
        assert!(ComputeStopObservation::from_checkpoint(&missing_orphan, *scope).is_err());
        let mut malformed = targets[1].0.clone();
        malformed["orphan_pod"] = "historical malformed physical identity".into();
        assert!(ComputeStopObservation::from_checkpoint(&malformed, *scope).is_err());
        let mut malformed = targets[1].0.clone();
        malformed["controller_uid"] = serde_json::json!({"old_format":4});
        assert!(ComputeStopObservation::from_checkpoint(&malformed, *scope).is_err());
    }

    #[test]
    fn stop_observation_does_not_add_binding_requirements_to_absence_or_application() {
        let targets = targets();
        for index in [0, 3] {
            for binding in [
                serde_json::json!({"old_authority": 4}),
                serde_json::json!("retired binding from an older version"),
            ] {
                let (checkpoint, scope) = &targets[index];
                let mut historical = checkpoint.clone();
                historical["resource_binding"] = binding;
                let original = historical.clone();
                assert!(ComputeStopObservation::from_checkpoint(&historical, *scope).is_ok());
                assert_eq!(historical, original);
            }
        }
    }

    #[test]
    fn stop_observation_accepts_non_absence_flags_on_retained_targets() {
        let targets = targets();
        for index in [2, 3, 1] {
            for absence in [serde_json::Value::Bool(false), serde_json::Value::Null] {
                let (checkpoint, scope) = &targets[index];
                let mut historical = checkpoint.clone();
                historical["compute_absent"] = absence;
                let original = historical.clone();
                assert!(ComputeStopObservation::from_checkpoint(&historical, *scope).is_ok());
                assert_eq!(historical, original);
            }
        }
    }

    #[test]
    fn stop_observation_ignores_controller_metadata_without_an_orphan_pod() {
        let targets = targets();
        for index in [2, 3, 0] {
            let (checkpoint, scope) = &targets[index];
            for metadata in [
                serde_json::json!("retired-controller"),
                serde_json::json!({"old_format": 4}),
            ] {
                let mut historical = checkpoint.clone();
                historical["controller_name"] = metadata.clone();
                historical["controller_uid"] = metadata;
                let original = historical.clone();
                assert!(ComputeStopObservation::from_checkpoint(&historical, *scope).is_ok());
                assert_eq!(historical, original);
            }
        }
    }
}
