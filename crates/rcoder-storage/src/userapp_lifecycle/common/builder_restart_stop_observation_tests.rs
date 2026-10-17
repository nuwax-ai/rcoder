//! DRAFT: real storage contracts, never runtime receipt/404 fabrication claims.
//! The engine/runtime fixture must separately hold the original live FD guard.
use super::ToastyUserAppStore;
use shared_types::*;

fn request(app: &UserAppLifecycleRecord, action: ComputeControlAction) -> ComputeControlRequest {
    let id = uuid::Uuid::new_v4().simple().to_string();
    ComputeControlRequest {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: UserAppOperationScope::Dev,
        operation_id: id.clone(),
        request_id: id,
        request_fingerprint: "a".repeat(64),
        action,
        restart_image_roll: false,
        builder_startup_mode: BuilderStartupMode::ManagementOnly,
    }
}

async fn captured_stopping(
    store: &ToastyUserAppStore,
    state: ComputeControlState,
) -> (ComputeControlRecord, BuilderRestartStopObservation) {
    captured_stopping_with_archive(store, state, true).await
}

async fn captured_stopping_with_archive(
    store: &ToastyUserAppStore,
    state: ComputeControlState,
    archive_present: bool,
) -> (ComputeControlRecord, BuilderRestartStopObservation) {
    let app = store
        .ensure_identity(&format!("stopbridge{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let pending = store
        .admit_compute_control(&request(&app, ComputeControlAction::Restart))
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: pending.app_id.clone(),
        lifecycle_id: pending.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "originalexecutor".into(),
    };
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let lease = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: 1,
        inode: 42,
        token: identity.executor_id.clone(),
    };
    let claimed = store.bind_compute_lease(&identity, &lease).await.unwrap();
    let target = BuilderControlTarget {
        context: UserAppExecutionContext {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            operation_id: pending.operation_id.clone(),
            executor_id: identity.executor_id.clone(),
            request_fingerprint: pending.request_fingerprint.clone(),
        },
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::Container,
            name: format!("rcoder-app-builder-{}", app.app_id),
            uid: "a".repeat(64),
            resource_version: None,
        }),
        pod: None,
        resource_binding: None,
        restart_image: Some("builder@sha256:original".into()),
        restart_runtime_workspace: None,
        builder_startup_mode: BuilderStartupMode::ManagementOnly,
    };
    let volumes = vec![AppResourceIdentity {
        kind: AppResourceKind::HostPath,
        name: "/home/user".into(),
        uid: "/owned-fixture/workspace".into(),
        resource_version: None,
    }];
    let mut checkpoint = serde_json::to_value(&target).unwrap();
    checkpoint["builder_volumes"] = serde_json::to_value(&volumes).unwrap();
    checkpoint["builder_restart_template"] = serde_json::to_value(BuilderRestartTemplate {
        source: target.clone(),
        archive: AppResourceIdentity {
            kind: AppResourceKind::File,
            name: "original-restart-archive".into(),
            uid: "b".repeat(64),
            resource_version: None,
        },
        volumes: volumes.clone(),
    })
    .unwrap();
    if !archive_present {
        checkpoint
            .as_object_mut()
            .unwrap()
            .remove("builder_restart_template");
    }
    let stopping = store
        .advance_compute_control(&ComputeControlProgress {
            identity: identity.clone(),
            expected_revision: claimed.revision,
            state: ComputeControlState::Running,
            stage: ComputeControlStage::Stopping,
            checkpoint,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    let record = match state {
        ComputeControlState::Running => stopping,
        ComputeControlState::RecoveryRequired => store
            .advance_compute_control(&ComputeControlProgress {
                identity,
                expected_revision: stopping.revision,
                state,
                stage: ComputeControlStage::Stopping,
                checkpoint: stopping.checkpoint,
                error_code: Some("ERR_STOP_REPLY_TIMEOUT".into()),
                error_message: Some("original Docker Stop response timed out".into()),
            })
            .await
            .unwrap(),
        ComputeControlState::Pending
        | ComputeControlState::Succeeded
        | ComputeControlState::Failed
        | ComputeControlState::Superseded => panic!("fixture state"),
    };
    (
        record,
        BuilderRestartStopObservation {
            target,
            volumes,
            kind: BuilderRestartStopKind::CapturedContainerAbsent,
        },
    )
}

async fn read(store: &ToastyUserAppStore, record: &ComputeControlRecord) -> ComputeControlRecord {
    store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap()
}

async fn positive_and_cas_contract(store: &ToastyUserAppStore) {
    for state in [
        ComputeControlState::Running,
        ComputeControlState::RecoveryRequired,
    ] {
        for kind in [
            BuilderRestartStopKind::StopReceiptAcknowledged,
            BuilderRestartStopKind::CapturedContainerAbsent,
        ] {
            let (snapshot, mut observation) = captured_stopping(store, state).await;
            observation.kind = kind;
            let (first, second) = tokio::join!(
                store.confirm_builder_restart_stopped(&snapshot, &observation),
                store.confirm_builder_restart_stopped(&snapshot, &observation),
            );
            let stopped = match (first, second) {
                (Ok(record), Err(_)) | (Err(_), Ok(record)) => record,
                results => panic!("exactly one stop-boundary CAS may win: {results:?}"),
            };
            assert_eq!(stopped.stage, "stopped");
            assert_eq!(stopped.state, snapshot.state);
            assert_eq!(stopped.revision, snapshot.revision + 1);
            assert_eq!(stopped.operation_id, snapshot.operation_id);
            assert_eq!(stopped.executor_id, snapshot.executor_id);
            assert_eq!(stopped.lease, snapshot.lease);
            assert_eq!(stopped.generation, snapshot.generation);
            assert_eq!(stopped.request_id, snapshot.request_id);
            assert_eq!(stopped.error_code, snapshot.error_code);
            assert_eq!(stopped.error_message, snapshot.error_message);
            let evidence: BuilderRestartStopRecoveryEvidence = serde_json::from_value(
                stopped.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY].clone(),
            )
            .unwrap();
            assert_eq!(evidence.observation, observation);
            assert_eq!(evidence.original_state, snapshot.state);
            assert_eq!(evidence.original_error_code, snapshot.error_code);
            assert_eq!(evidence.original_error_message, snapshot.error_message);
            let mut without_evidence = stopped.checkpoint.clone();
            without_evidence
                .as_object_mut()
                .unwrap()
                .remove(BUILDER_RESTART_STOP_OBSERVATION_KEY);
            assert_eq!(
                without_evidence, snapshot.checkpoint,
                "archive/target/volumes unchanged"
            );
            assert!(
                store
                    .confirm_builder_restart_stopped(&snapshot, &observation)
                    .await
                    .is_err()
            );

            // This ledger contract performs no Start/Create. The runtime
            // contract must prove only the winning CAS proceeds to restore.
            let resumed = store
                .resume_builder_restart_start(&stopped, &observation.target)
                .await
                .unwrap();
            assert_eq!(resumed.stage, "starting");
            assert_eq!(resumed.state, ComputeControlState::Running);
            assert!(resumed.error_code.is_none() && resumed.error_message.is_none());
            assert_eq!(
                resumed.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY],
                stopped.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY]
            );
            assert_eq!(
                resumed.checkpoint["builder_restart_template"],
                snapshot.checkpoint["builder_restart_template"]
            );
            assert_eq!(resumed.lease, snapshot.lease);
        }
    }
}

async fn altered_evidence_contract(store: &ToastyUserAppStore) {
    let (snapshot, observation) =
        captured_stopping(store, ComputeControlState::RecoveryRequired).await;
    for change in [
        "cid",
        "short-cid",
        "name",
        "context",
        "image",
        "volume",
        "empty-volumes",
        "volume-kind",
        "pod",
    ] {
        let mut forged = observation.clone();
        match change {
            "cid" => forged.target.workload.as_mut().unwrap().uid = "c".repeat(64),
            "short-cid" => forged.target.workload.as_mut().unwrap().uid = "a".repeat(12),
            "name" => forged.target.workload.as_mut().unwrap().name = "another-builder".into(),
            "context" => forged.target.context.operation_id = "another-operation".into(),
            "image" => forged.target.restart_image = Some("another-image".into()),
            "volume" => forged.volumes[0].uid = "/another-volume".into(),
            "empty-volumes" => forged.volumes.clear(),
            "volume-kind" => forged.volumes[0].kind = AppResourceKind::PersistentVolumeClaim,
            "pod" => {
                forged.target.pod = Some(BuilderPodIdentity {
                    name: "unexpected-pod".into(),
                    uid: "unexpected-uid".into(),
                    resource_version: "1".into(),
                })
            }
            _ => unreachable!(),
        }
        assert!(
            store
                .confirm_builder_restart_stopped(&snapshot, &forged)
                .await
                .is_err(),
            "{change}"
        );
        assert_eq!(
            read(store, &snapshot).await,
            snapshot,
            "{change} must not modify the ledger"
        );
    }
    for change in [
        "lease", "archive", "volumes", "executor", "scope", "state", "stage", "action",
    ] {
        let mut stale = snapshot.clone();
        match change {
            "lease" => {
                if let Some(UserAppOperationLeaseReceipt::Docker { inode, .. }) = &mut stale.lease {
                    *inode += 1;
                }
            }
            "archive" => {
                stale.checkpoint["builder_restart_template"]["archive"]["uid"] =
                    "d".repeat(64).into()
            }
            "volumes" => stale.checkpoint["builder_volumes"][0]["uid"] = "/changed".into(),
            "executor" => stale.executor_id = Some("differentexecutor".into()),
            "scope" => stale.scope = UserAppOperationScope::Prod,
            "state" => stale.state = ComputeControlState::Succeeded,
            "stage" => stale.stage = "starting".into(),
            "action" => stale.action = ComputeControlAction::Stop,
            _ => unreachable!(),
        }
        assert!(
            store
                .confirm_builder_restart_stopped(&stale, &observation)
                .await
                .is_err(),
            "{change}"
        );
        assert_eq!(
            read(store, &snapshot).await,
            snapshot,
            "{change} must preserve original history"
        );
    }
}

async fn superseding_stop_contract(store: &ToastyUserAppStore) {
    let (snapshot, observation) =
        captured_stopping(store, ComputeControlState::RecoveryRequired).await;
    let app = store
        .get_application(&snapshot.app_id)
        .await
        .unwrap()
        .unwrap();
    let newer = store
        .admit_compute_control(&request(&app, ComputeControlAction::Stop))
        .await
        .unwrap();
    let old_after_admission = read(store, &snapshot).await;
    assert_eq!(old_after_admission.state, ComputeControlState::Superseded);
    assert!(
        store
            .confirm_builder_restart_stopped(&snapshot, &observation)
            .await
            .is_err()
    );
    assert_eq!(read(store, &snapshot).await, old_after_admission);
    assert_eq!(
        read(store, &newer).await,
        newer,
        "cannot close a superseding Stop"
    );
}

async fn contract(store: &ToastyUserAppStore) {
    positive_and_cas_contract(store).await;
    altered_evidence_contract(store).await;
    superseding_stop_contract(store).await;
    existing_continuation_restores_before_starting_cas_contract(store).await;
    opaque_observation_never_blocks_existing_stopped_contract(store).await;
    failed_old_recipe_allows_current_source_contract(store).await;
    positive_private_archive_unavailability_contract(store).await;
}

fn executor(record: &ComputeControlRecord) -> ComputeExecutorIdentity {
    ComputeExecutorIdentity {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        operation_id: record.operation_id.clone(),
        scope: record.scope,
        generation: record.generation,
        executor_id: record.executor_id.clone().unwrap(),
    }
}

async fn existing_continuation_restores_before_starting_cas_contract(store: &ToastyUserAppStore) {
    let (snapshot, observation) =
        captured_stopping(store, ComputeControlState::RecoveryRequired).await;
    let stopped = store
        .confirm_builder_restart_stopped(&snapshot, &observation)
        .await
        .unwrap();
    let mut restored = observation.target.clone();
    restored.workload.as_mut().unwrap().uid = "c".repeat(64);
    // Existing runtime restore returns an actually observed stopped Container
    // before this CAS; the storage fixture supplies that observation only.
    let (first, second) = tokio::join!(
        store.resume_builder_restart_start(&stopped, &restored),
        store.resume_builder_restart_start(&stopped, &restored)
    );
    let starting = match (first, second) {
        (Ok(record), Err(_)) | (Err(_), Ok(record)) => record,
        results => panic!("only one unchanged Stopped snapshot may continue: {results:?}"),
    };
    assert_eq!(starting.stage, "starting");
    assert_eq!(
        BuilderControlTarget::from_observation_checkpoint(&starting.checkpoint).unwrap(),
        restored
    );
    assert_eq!(starting.lease, snapshot.lease);
    assert_eq!(starting.operation_id, snapshot.operation_id);
    assert_eq!(
        starting.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY],
        stopped.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY]
    );
    assert_eq!(
        starting.checkpoint["builder_restart_template"],
        stopped.checkpoint["builder_restart_template"]
    );
    assert!(
        store
            .resume_builder_restart_start(&stopped, &restored)
            .await
            .is_err()
    );
}

async fn opaque_observation_never_blocks_existing_stopped_contract(store: &ToastyUserAppStore) {
    for raw in [
        None,
        Some(serde_json::json!({"legacy_unreadable": ["retain", 7]})),
    ] {
        let (snapshot, observation) = captured_stopping(store, ComputeControlState::Running).await;
        let mut checkpoint = snapshot.checkpoint.clone();
        if let Some(value) = &raw {
            checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY] = value.clone();
        }
        // A storage fixture supplies its existing acknowledged Stopped boundary;
        // it does not manufacture a runtime ACK or derive it from this JSON.
        let stopped = store
            .advance_compute_control(&ComputeControlProgress {
                identity: executor(&snapshot),
                expected_revision: snapshot.revision,
                state: ComputeControlState::Running,
                stage: ComputeControlStage::Stopped,
                checkpoint,
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
        let resumed = store
            .resume_builder_restart_start(&stopped, &observation.target)
            .await
            .unwrap();
        assert_eq!(
            resumed.checkpoint.get(BUILDER_RESTART_STOP_OBSERVATION_KEY),
            raw.as_ref()
        );
        assert_eq!(resumed.stage, "starting");
        assert_eq!(resumed.lease, stopped.lease);
    }
    let (snapshot, observation) = captured_stopping(store, ComputeControlState::Running).await;
    let historical = serde_json::json!({"old":"opaque stop observation","nested":[1,2]});
    let mut checkpoint = snapshot.checkpoint.clone();
    checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY] = historical.clone();
    let prior = store
        .advance_compute_control(&ComputeControlProgress {
            identity: executor(&snapshot),
            expected_revision: snapshot.revision,
            state: ComputeControlState::Running,
            stage: ComputeControlStage::Stopping,
            checkpoint,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    let confirmed = store
        .confirm_builder_restart_stopped(&prior, &observation)
        .await
        .unwrap();
    let evidence: BuilderRestartStopRecoveryEvidence =
        serde_json::from_value(confirmed.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY].clone())
            .unwrap();
    assert_eq!(evidence.previous_observation, Some(historical));
    assert_eq!(
        confirmed.checkpoint["builder_restart_template"],
        snapshot.checkpoint["builder_restart_template"]
    );
}

async fn failed_old_recipe_allows_current_source_contract(store: &ToastyUserAppStore) {
    for stage in ["stopping", "stopped"] {
        for opaque in [false, true] {
            let (mut snapshot, observation) =
                captured_stopping_with_archive(store, ComputeControlState::Running, false).await;
            if opaque {
                let mut checkpoint = snapshot.checkpoint.clone();
                checkpoint["builder_restart_template"] =
                    serde_json::json!({"retained":"old unreadable recipe"});
                snapshot = store
                    .advance_compute_control(&ComputeControlProgress {
                        identity: executor(&snapshot),
                        expected_revision: snapshot.revision,
                        state: ComputeControlState::Running,
                        stage: ComputeControlStage::Stopping,
                        checkpoint,
                        error_code: None,
                        error_message: None,
                    })
                    .await
                    .unwrap();
            }
            let historical = serde_json::json!({
                "retained": "original opaque Stop observation",
                "body": ["must", "survive", 17]
            });
            let mut original_checkpoint = snapshot.checkpoint.clone();
            original_checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY] = historical.clone();
            snapshot = store
                .advance_compute_control(&ComputeControlProgress {
                    identity: executor(&snapshot),
                    expected_revision: snapshot.revision,
                    state: ComputeControlState::RecoveryRequired,
                    stage: ComputeControlStage::Stopping,
                    checkpoint: original_checkpoint,
                    error_code: Some("ERR_STOP_REPLY_TIMEOUT".into()),
                    error_message: Some("original diagnostic remains unchanged".into()),
                })
                .await
                .unwrap();
            if stage == "stopped" {
                snapshot = store
                    .confirm_builder_restart_stopped(&snapshot, &observation)
                    .await
                    .unwrap();
                assert!(
                    store
                        .fail_unrebuildable_builder_restart(&snapshot, &observation, None)
                        .await
                        .is_err(),
                    "a committed typed Stopped boundary may hide an unknown Create"
                );
                assert_eq!(read(store, &snapshot).await, snapshot);
                for history in [None, Some(historical.clone())] {
                    // Progress cannot edit an already RecoveryRequired row.
                    // Persist each legacy shape through a fresh legal Running
                    // -> RecoveryRequired/Stopped transition instead.
                    let (active, legacy_observation) =
                        captured_stopping_with_archive(store, ComputeControlState::Running, false)
                            .await;
                    let mut checkpoint = active.checkpoint.clone();
                    if opaque {
                        checkpoint["builder_restart_template"] =
                            serde_json::json!({"retained":"old unreadable recipe"});
                    }
                    if let Some(value) = history {
                        checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY] = value;
                    }
                    let legacy = store
                        .advance_compute_control(&ComputeControlProgress {
                            identity: executor(&active),
                            expected_revision: active.revision,
                            state: ComputeControlState::RecoveryRequired,
                            stage: ComputeControlStage::Stopped,
                            checkpoint,
                            error_code: Some("ERR_STOP_REPLY_TIMEOUT".into()),
                            error_message: Some("original diagnostic remains unchanged".into()),
                        })
                        .await
                        .unwrap();
                    assert!(
                        store
                            .fail_unrebuildable_builder_restart(&legacy, &legacy_observation, None)
                            .await
                            .is_err(),
                        "Stopped cannot be terminalized from missing/opaque history"
                    );
                    assert_eq!(read(store, &legacy).await, legacy);
                }
                continue;
            }
            assert_eq!(snapshot.stage, "stopping");
            let before_status = store
                .read_compute_status(&snapshot.app_id, &snapshot.lifecycle_id, snapshot.scope)
                .await
                .unwrap();
            let (first, second) = tokio::join!(
                store.fail_unrebuildable_builder_restart(&snapshot, &observation, None),
                store.fail_unrebuildable_builder_restart(&snapshot, &observation, None),
            );
            let failed = match (first, second) {
                (Ok(record), Err(_)) | (Err(_), Ok(record)) => record,
                results => panic!("one exact settlement wins: {results:?}"),
            };
            assert_eq!(failed.state, ComputeControlState::Failed);
            assert_eq!(failed.stage, "stopped");
            assert_eq!(failed.error_code, snapshot.error_code);
            assert_eq!(failed.error_message, snapshot.error_message);
            assert_eq!(
                failed.lease, snapshot.lease,
                "runtime still must release the original FD guard"
            );
            let evidence: BuilderRestartStopRecoveryEvidence = serde_json::from_value(
                failed.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY].clone(),
            )
            .unwrap();
            assert_eq!(
                failed.revision,
                snapshot.revision + 1,
                "direct Stopping terminal CAS"
            );
            assert_eq!(evidence.original_state, snapshot.state);
            assert_eq!(evidence.original_error_code, snapshot.error_code);
            assert_eq!(evidence.original_error_message, snapshot.error_message);
            assert_eq!(evidence.previous_observation, Some(historical.clone()));
            let mut retained_checkpoint = failed.checkpoint.clone();
            retained_checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY] = historical;
            assert_eq!(
                retained_checkpoint, snapshot.checkpoint,
                "original raw key/body and recipe retained"
            );
            let unrebuildable = evidence.unrebuildable.unwrap();
            assert_eq!(unrebuildable.observation, observation);
            assert_eq!(
                unrebuildable.reason,
                if opaque {
                    BuilderRestartUnrebuildableReason::UnreadableArchive
                } else {
                    BuilderRestartUnrebuildableReason::MissingArchive
                }
            );
            let status = store
                .read_compute_status(&failed.app_id, &failed.lifecycle_id, failed.scope)
                .await
                .unwrap();
            assert_eq!(status.generation, before_status.generation);
            assert_eq!(
                status.revision, before_status.revision,
                "head intent was not replaced"
            );
            assert!(!status.desired_stopped);
            assert_eq!(status.operation, Some(failed.clone()));
            // An outer failure reporter cannot turn the committed old Failed
            // closure back into RecoveryRequired or overwrite its timeout.
            assert!(
                store
                    .advance_compute_control(&ComputeControlProgress {
                        identity: executor(&failed),
                        expected_revision: failed.revision,
                        state: ComputeControlState::RecoveryRequired,
                        stage: ComputeControlStage::Stopped,
                        checkpoint: failed.checkpoint.clone(),
                        error_code: Some("replacement-code".into()),
                        error_message: Some("replacement diagnostic".into()),
                    })
                    .await
                    .is_err()
            );
            assert_eq!(read(store, &failed).await, failed);

            let identity = executor(&failed);
            store
                .forget_compute_lease(&identity, failed.lease.as_ref().unwrap())
                .await
                .unwrap();
            store
                .check_compute_access(&failed.app_id, failed.scope, false)
                .await
                .unwrap();
            assert!(
                store
                    .get_application(&failed.app_id)
                    .await
                    .unwrap()
                    .is_some()
            );
            let cursor = &failed.app_id[..failed.app_id.len() - 1];
            assert!(
                store
                    .list_applications(Some(cursor), 100)
                    .await
                    .unwrap()
                    .iter()
                    .any(|app| app.app_id == failed.app_id)
            );
            let admitted = store
                .admit(&UserAppAdmission {
                    app_id: failed.app_id.clone(),
                    lifecycle_id: Some(failed.lifecycle_id.clone()),
                    operation_id: uuid::Uuid::new_v4().simple().to_string(),
                    request_id: Some(uuid::Uuid::new_v4().simple().to_string()),
                    request_fingerprint: "f".repeat(64),
                    kind: UserAppOperationKind::EnsureBuilder,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                })
                .await
                .unwrap();
            assert!(
                matches!(admitted, UserAppAdmissionOutcome::Accepted(_)),
                "a complete current Source does not need the failed old recipe"
            );
            assert_eq!(
                read(store, &failed).await.error_message,
                snapshot.error_message
            );
        }
    }
    let (snapshot, observation) = captured_stopping(store, ComputeControlState::Running).await;
    assert!(
        store
            .fail_unrebuildable_builder_restart(&snapshot, &observation, None)
            .await
            .is_err(),
        "a valid archive must continue its original restart"
    );
    let stopped = store
        .confirm_builder_restart_stopped(&snapshot, &observation)
        .await
        .unwrap();
    let starting = store
        .resume_builder_restart_start(&stopped, &observation.target)
        .await
        .unwrap();
    assert!(
        store
            .fail_unrebuildable_builder_restart(&starting, &observation, None)
            .await
            .is_err(),
        "Starting may contain an unknown Create; old-CID absence cannot settle it"
    );
    let (missing, mut observed) =
        captured_stopping_with_archive(store, ComputeControlState::Running, false).await;
    observed.kind = BuilderRestartStopKind::StopReceiptAcknowledged;
    assert!(
        store
            .fail_unrebuildable_builder_restart(&missing, &observed, None)
            .await
            .is_err(),
        "a Stop ACK does not prove the captured old CID is absent"
    );
    observed.kind = BuilderRestartStopKind::CapturedContainerAbsent;
    let mut unknown_creation = missing;
    for stage in [ComputeControlStage::Stopped, ComputeControlStage::Starting] {
        unknown_creation = store
            .advance_compute_control(&ComputeControlProgress {
                identity: executor(&unknown_creation),
                expected_revision: unknown_creation.revision,
                state: ComputeControlState::Running,
                stage,
                checkpoint: unknown_creation.checkpoint.clone(),
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
        assert!(
            store
                .fail_unrebuildable_builder_restart(&unknown_creation, &observed, None)
                .await
                .is_err(),
            "Stopped/Starting without history cannot prove that Create was never dispatched"
        );
        assert_eq!(read(store, &unknown_creation).await, unknown_creation);
    }
    assert!(
        unknown_creation
            .checkpoint
            .get("builder_restart_template")
            .is_none()
    );
    assert!(
        store
            .fail_unrebuildable_builder_restart(&unknown_creation, &observed, None)
            .await
            .is_err(),
        "even a missing recipe cannot settle an original Starting/unknown-Create record"
    );
    assert_eq!(read(store, &unknown_creation).await, unknown_creation);
}

async fn positive_private_archive_unavailability_contract(store: &ToastyUserAppStore) {
    for stage in ["stopping", "stopped"] {
        let (mut snapshot, observation) =
            captured_stopping(store, ComputeControlState::RecoveryRequired).await;
        if stage == "stopped" {
            snapshot = store
                .confirm_builder_restart_stopped(&snapshot, &observation)
                .await
                .unwrap();
        }
        let reference = snapshot.checkpoint["builder_restart_template"].clone();
        let unavailable = BuilderRestartArchiveUnavailable {
            template: serde_json::from_value(reference.clone()).unwrap(),
            kind: BuilderRestartArchiveUnavailableKind::PositiveUnavailable,
        };
        // A storage fixture supplies the typed result of runtime Some(false).
        // Runtime Err/None/Some(true) must not manufacture this witness.
        assert!(
            store
                .fail_unrebuildable_builder_restart(&snapshot, &observation, None)
                .await
                .is_err(),
            "an unknown private recipe result cannot end the original restart"
        );
        if stage == "stopped" {
            assert!(
                store
                    .fail_unrebuildable_builder_restart(&snapshot, &observation, Some(&unavailable))
                    .await
                    .is_err(),
                "even positive archive unavailability cannot settle Stopped with possible unknown Create"
            );
            assert_eq!(read(store, &snapshot).await, snapshot);
            continue;
        }
        assert_eq!(snapshot.stage, "stopping");
        for change in ["reference", "source", "volumes"] {
            let mut wrong = unavailable.clone();
            match change {
                "reference" => wrong.template.archive.uid = "f".repeat(64),
                "source" => wrong.template.source.workload.as_mut().unwrap().uid = "e".repeat(64),
                "volumes" => wrong.template.volumes[0].uid = "/foreign-mount".into(),
                _ => unreachable!(),
            }
            assert!(
                store
                    .fail_unrebuildable_builder_restart(&snapshot, &observation, Some(&wrong))
                    .await
                    .is_err(),
                "{change}"
            );
            assert_eq!(read(store, &snapshot).await, snapshot);
        }
        let failed = store
            .fail_unrebuildable_builder_restart(&snapshot, &observation, Some(&unavailable))
            .await
            .unwrap();
        assert_eq!(
            failed.revision,
            snapshot.revision + 1,
            "direct Stopping terminal CAS"
        );
        assert_eq!(failed.state, ComputeControlState::Failed);
        assert_eq!(failed.stage, "stopped");
        assert_eq!(
            failed.checkpoint["builder_restart_template"], reference,
            "positive unavailability never deletes the original valid reference"
        );
        assert_eq!(failed.error_code, snapshot.error_code);
        assert_eq!(failed.error_message, snapshot.error_message);
        let evidence: BuilderRestartStopRecoveryEvidence =
            serde_json::from_value(failed.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY].clone())
                .unwrap();
        let terminal = evidence.unrebuildable.unwrap();
        assert_eq!(
            terminal.reason,
            BuilderRestartUnrebuildableReason::ArchiveUnavailable
        );
        assert_eq!(terminal.archive_unavailable, Some(unavailable));
        store
            .forget_compute_lease(&executor(&failed), failed.lease.as_ref().unwrap())
            .await
            .unwrap();
        store
            .check_compute_access(&failed.app_id, failed.scope, false)
            .await
            .unwrap();
        let outcome = store
            .admit(&UserAppAdmission {
                app_id: failed.app_id.clone(),
                lifecycle_id: Some(failed.lifecycle_id.clone()),
                operation_id: uuid::Uuid::new_v4().simple().to_string(),
                request_id: None,
                request_fingerprint: "f".repeat(64),
                kind: UserAppOperationKind::EnsureBuilder,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .unwrap();
        assert!(matches!(outcome, UserAppAdmissionOutcome::Accepted(_)));
    }
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn turso_restart_stop_observation_preserves_original_intent_and_rejects_stale_authority() {
    let directory = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&directory.path().join("stop-observation.db"))
        .await
        .unwrap();
    contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicitly disposable PostgreSQL DSN; draft has not been executed"]
async fn pg_restart_stop_observation_preserves_original_intent_and_rejects_stale_authority() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG DSN required")),
        max_connections: Some(1),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    contract(&store).await;
    store.shutdown().await.unwrap();
}
