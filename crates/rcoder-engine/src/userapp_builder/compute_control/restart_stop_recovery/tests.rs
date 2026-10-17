//! Pure eligibility regressions only: no runtime, process, storage or network.
//! The parent module registers this draft test module.
use super::*;

fn stopped_record(kind: BuilderRestartStopKind) -> ComputeControlRecord {
    let context = UserAppExecutionContext {
        app_id: "eligibleapp".into(),
        lifecycle_id: "originallife".into(),
        operation_id: "originalrestart".into(),
        executor_id: "originalexecutor".into(),
        request_fingerprint: "a".repeat(64),
    };
    let target = BuilderControlTarget {
        context: context.clone(),
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::Container,
            name: "rcoder-app-builder-eligibleapp".into(),
            uid: "a".repeat(64),
            resource_version: None,
        }),
        pod: None,
        resource_binding: None,
        restart_image: Some("original-builder-image".into()),
        restart_runtime_workspace: None,
        builder_startup_mode: BuilderStartupMode::PreserveBusinessIntent,
    };
    let volumes = vec![AppResourceIdentity {
        kind: AppResourceKind::HostPath,
        name: "/home/user/eligibleapp".into(),
        uid: "/fixture/original-workspace".into(),
        resource_version: None,
    }];
    let reference = serde_json::to_value(BuilderRestartTemplate {
        source: target.clone(),
        archive: AppResourceIdentity {
            kind: AppResourceKind::File,
            name: "original-recipe".into(),
            uid: "b".repeat(64),
            resource_version: None,
        },
        volumes: volumes.clone(),
    })
    .unwrap();
    let mut checkpoint = serde_json::to_value(&target).unwrap();
    checkpoint["builder_volumes"] = serde_json::to_value(&volumes).unwrap();
    checkpoint["builder_restart_template"] = reference.clone();
    checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY] =
        serde_json::to_value(BuilderRestartStopRecoveryEvidence {
            observation: BuilderRestartStopObservation {
                target,
                volumes,
                kind,
            },
            original_state: ComputeControlState::RecoveryRequired,
            original_error_code: Some("ERR_STOP_REPLY_TIMEOUT".into()),
            original_error_message: Some("original timeout diagnostic".into()),
            original_restart_template: Some(reference),
            unrebuildable: None,
            previous_observation: None,
        })
        .unwrap();
    ComputeControlRecord {
        created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        app_id: context.app_id,
        lifecycle_id: context.lifecycle_id,
        request_id: "originalrequest".into(),
        scope: UserAppOperationScope::Dev,
        operation_id: context.operation_id,
        request_fingerprint: context.request_fingerprint,
        generation: 1,
        revision: 5,
        action: ComputeControlAction::Restart,
        state: ComputeControlState::RecoveryRequired,
        executor_id: Some(context.executor_id.clone()),
        stage: ComputeControlStage::Stopped.as_str().into(),
        checkpoint,
        error_code: Some("ERR_STOP_REPLY_TIMEOUT".into()),
        error_message: Some("original timeout diagnostic".into()),
        lease: Some(UserAppOperationLeaseReceipt::Docker {
            service_type: ServiceType::UserappBuilder,
            device: 1,
            inode: 42,
            token: context.executor_id,
        }),
        interrupted_operations: vec![],
    }
}

#[test]
fn stopped_missing_opaque_or_semantically_mismatched_observation_keeps_the_original_path() {
    for case in [
        "missing",
        "invalid-json",
        "target",
        "mounts",
        "context",
        "reference",
        "old-state",
        "short-cid",
        "empty-mounts",
    ] {
        let mut record = stopped_record(BuilderRestartStopKind::CapturedContainerAbsent);
        match case {
            "missing" => {
                record
                    .checkpoint
                    .as_object_mut()
                    .unwrap()
                    .remove(BUILDER_RESTART_STOP_OBSERVATION_KEY);
            }
            "invalid-json" => {
                record.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY] =
                    serde_json::json!("{retained opaque non-json observation");
            }
            "target" => {
                record.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY]["observation"]["target"]
                    ["workload"]["uid"] = "c".repeat(64).into();
            }
            "mounts" => {
                record.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY]["observation"]["volumes"]
                    [0]["uid"] = "/fixture/another-workspace".into();
            }
            "context" => {
                record.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY]["observation"]["target"]
                    ["context"]["lifecycle_id"] = "foreignlife".into();
            }
            "reference" => {
                record.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY]["original_restart_template"]
                    ["archive"]["uid"] = "d".repeat(64).into();
            }
            "old-state" => {
                record.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY]["original_state"] =
                    serde_json::to_value(ComputeControlState::Succeeded).unwrap();
            }
            "short-cid" => {
                record.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY]["observation"]["target"]
                    ["workload"]["uid"] = "a".repeat(12).into();
            }
            "empty-mounts" => {
                record.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY]["observation"]["volumes"] =
                    serde_json::json!([]);
            }
            _ => unreachable!("enumerated test case"),
        }
        assert!(
            record.has_docker_compute_target(),
            "{case}: diagnostic damage must not invalidate the unchanged original Docker target"
        );
        assert!(
            !eligible(&record),
            "{case}: the original supported Stopped continuation must retain dispatch"
        );
    }
}

#[test]
fn exact_stopped_and_starting_history_never_steal_existing_continuation_semantics() {
    for kind in [
        BuilderRestartStopKind::StopReceiptAcknowledged,
        BuilderRestartStopKind::CapturedContainerAbsent,
    ] {
        let mut record = stopped_record(kind);
        let evidence: BuilderRestartStopRecoveryEvidence =
            serde_json::from_value(record.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY].clone())
                .unwrap();
        assert_eq!(evidence.observation.kind, kind);
        assert!(
            !eligible(&record),
            "an old Stopped record may contain an unknown Create and keeps its existing continuation"
        );
        let original_checkpoint = record.checkpoint.clone();
        record.stage = ComputeControlStage::Starting.as_str().into();
        assert!(
            !eligible(&record),
            "Starting stays on existing result-observation/continuation semantics"
        );
        assert_eq!(record.checkpoint, original_checkpoint);
        record.stage = ComputeControlStage::Stopping.as_str().into();
        assert!(
            eligible(&record),
            "only the original pre-restore Stop boundary is newly recoverable"
        );
    }
}
