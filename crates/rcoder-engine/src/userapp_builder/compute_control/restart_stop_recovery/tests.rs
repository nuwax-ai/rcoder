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

fn kubernetes_stopping_record() -> ComputeControlRecord {
    let context = UserAppExecutionContext {
        app_id: "stsapp".into(),
        lifecycle_id: "stsoriginal".into(),
        operation_id: "stsoriginalrestart".into(),
        executor_id: "stsoriginalexecutor".into(),
        request_fingerprint: "a".repeat(64),
    };
    let target = BuilderControlTarget {
        context: context.clone(),
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: "rcoder-app-builder-stsapp".into(),
            uid: "d".repeat(36),
            resource_version: None,
        }),
        pod: None,
        resource_binding: None,
        restart_image: Some("builder@sha256:original".into()),
        restart_runtime_workspace: None,
        builder_startup_mode: BuilderStartupMode::ManagementOnly,
    };
    let mut checkpoint = serde_json::to_value(&target).unwrap();
    checkpoint["builder_volumes"] = serde_json::to_value(vec![AppResourceIdentity {
        kind: AppResourceKind::PersistentVolumeClaim,
        name: "builder-workspace".into(),
        uid: "builder-workspace".into(),
        resource_version: None,
    }])
    .unwrap();
    ComputeControlRecord {
        created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        app_id: context.app_id,
        lifecycle_id: context.lifecycle_id,
        request_id: "stsoriginalrequest".into(),
        scope: UserAppOperationScope::Dev,
        operation_id: context.operation_id,
        request_fingerprint: context.request_fingerprint,
        generation: 1,
        revision: 5,
        action: ComputeControlAction::Restart,
        state: ComputeControlState::Running,
        executor_id: Some(context.executor_id.clone()),
        stage: ComputeControlStage::Stopping.as_str().into(),
        checkpoint,
        error_code: None,
        error_message: None,
        lease: Some(UserAppOperationLeaseReceipt::Kubernetes {
            service_type: ServiceType::UserappBuilder,
            namespace: "rcoder-builder".into(),
            name: "rcoder-app-builder-stsapp".into(),
            uid: "c".repeat(36),
            resource_version: "12".into(),
            token: context.executor_id,
        }),
        interrupted_operations: vec![],
    }
}

#[test]
fn kubernetes_stopping_boundary_eligibility_is_exact() {
    let record = kubernetes_stopping_record();
    assert!(
        kubernetes_eligible(&record),
        "a StatefulSet Restart stuck at Stopping is the newly recovered boundary"
    );

    let mut docker_domain = kubernetes_stopping_record();
    let target =
        BuilderControlTarget::from_observation_checkpoint(&docker_domain.checkpoint).unwrap();
    let mut docker_target = target.clone();
    docker_target.workload = Some(AppResourceIdentity {
        kind: AppResourceKind::Container,
        name: target.workload.clone().unwrap().name,
        uid: "a".repeat(64),
        resource_version: None,
    });
    docker_domain.checkpoint = serde_json::to_value(&docker_target).unwrap();
    assert!(
        !kubernetes_eligible(&docker_domain),
        "Container targets keep the original Docker FD recovery domain"
    );

    for (stage, expect) in [
        (ComputeControlStage::Stopped, false),
        (ComputeControlStage::Starting, false),
        (ComputeControlStage::Verifying, false),
        (ComputeControlStage::Stopping, true),
    ] {
        let mut staged = kubernetes_stopping_record();
        staged.stage = stage.as_str().into();
        assert_eq!(kubernetes_eligible(&staged), expect, "{stage:?}");
    }

    for state in [
        ComputeControlState::RecoveryRequired,
        ComputeControlState::Running,
    ] {
        let mut stated = kubernetes_stopping_record();
        stated.state = state;
        assert!(kubernetes_eligible(&stated), "{state:?} is recoverable");
    }
    for state in [
        ComputeControlState::Pending,
        ComputeControlState::Succeeded,
        ComputeControlState::Failed,
        ComputeControlState::Superseded,
    ] {
        let mut stated = kubernetes_stopping_record();
        stated.state = state;
        assert!(!kubernetes_eligible(&stated), "{state:?} is out of domain");
    }

    let mut scoped = kubernetes_stopping_record();
    scoped.scope = UserAppOperationScope::Prod;
    assert!(
        !kubernetes_eligible(&scoped),
        "prod restart is out of domain"
    );

    let mut acted = kubernetes_stopping_record();
    acted.action = ComputeControlAction::Stop;
    assert!(
        !kubernetes_eligible(&acted),
        "stop actions have their own paths"
    );

    let mut contextless = kubernetes_stopping_record();
    let mut other_context =
        BuilderControlTarget::from_observation_checkpoint(&contextless.checkpoint).unwrap();
    other_context.context.operation_id = "adifferentoperation".into();
    contextless.checkpoint = serde_json::to_value(&other_context).unwrap();
    assert!(
        !kubernetes_eligible(&contextless),
        "the checkpoint target must belong to this exact operation"
    );

    let mut missing = kubernetes_stopping_record();
    let mut no_workload =
        BuilderControlTarget::from_observation_checkpoint(&missing.checkpoint).unwrap();
    no_workload.workload = None;
    missing.checkpoint = serde_json::to_value(&no_workload).unwrap();
    assert!(
        !kubernetes_eligible(&missing),
        "a missing-compute stopping shape has no stop-side evidence to reconcile"
    );
}
