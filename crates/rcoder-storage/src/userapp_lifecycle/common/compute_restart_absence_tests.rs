//! Original-operation recovery when compute is absent and data is retained.
use super::*;

async fn absent_builder_restart_continuation_contract(store: &ToastyUserAppStore) {
    for interrupted_start in [false, true] {
        let app = store
            .ensure_identity(&format!("absentrestart{}", uuid::Uuid::new_v4().simple()))
            .await
            .unwrap();
        let pending = store
            .admit_compute_control(&request(
                &app,
                &format!("restart{}", uuid::Uuid::new_v4().simple()),
                ComputeControlAction::Restart,
            ))
            .await
            .unwrap();
        let identity = compute_identity(&pending);
        store
            .claim_compute_control(&identity, pending.revision)
            .await
            .unwrap();
        let lease = compute_receipt();
        let mut record = store.bind_compute_lease(&identity, &lease).await.unwrap();
        let target = BuilderControlTarget {
            context: record.execution_context().unwrap(),
            workload: None,
            pod: None,
            resource_binding: None,
            restart_image: Some("frozen-runner-image".into()),
            restart_runtime_workspace: None,
            builder_startup_mode: BuilderStartupMode::ManagementOnly,
        };
        let volume = AppResourceIdentity {
            kind: AppResourceKind::PersistentVolumeClaim,
            name: format!("rcoder-app-builder-{}-workspace", app.app_id),
            uid: "retained-pvc-uid".into(),
            resource_version: None,
        };
        let mut checkpoint = serde_json::to_value(&target).unwrap();
        checkpoint["builder_volumes"] = serde_json::json!([volume]);
        for stage in [ComputeControlStage::Stopping, ComputeControlStage::Stopped] {
            let mut progress = compute_progress(&record, stage);
            progress.checkpoint = checkpoint.clone();
            record = store.advance_compute_control(&progress).await.unwrap();
        }
        if interrupted_start {
            let mut progress = compute_progress(&record, ComputeControlStage::Starting);
            progress.checkpoint = checkpoint.clone();
            record = store.advance_compute_control(&progress).await.unwrap();
            let mut progress = compute_progress(&record, ComputeControlStage::Starting);
            progress.checkpoint = checkpoint.clone();
            progress.state = ComputeControlState::RecoveryRequired;
            progress.error_code = Some("ERR_BACKEND_ERROR".into());
            progress.error_message = Some("creation response lost".into());
            record = store.advance_compute_control(&progress).await.unwrap();
        }
        for field in ["image", "mode", "workload"] {
            let mut changed = target.clone();
            match field {
                "image" => changed.restart_image = Some("later-image".into()),
                "mode" => changed.builder_startup_mode = Default::default(),
                "workload" => {
                    changed.workload = Some(AppResourceIdentity {
                        kind: AppResourceKind::StatefulSet,
                        name: "invented-workload".into(),
                        uid: "invented-old-uid".into(),
                        resource_version: Some("1".into()),
                    });
                }
                _ => unreachable!(),
            }
            assert!(
                store
                    .resume_builder_restart_start(&record, &changed)
                    .await
                    .is_err()
            );
        }
        for volumes in [
            serde_json::json!([]),
            serde_json::json!([{"kind":"PersistentVolumeClaim","name":"workspace","uid":"","resource_version":null}]),
            serde_json::json!([volume.clone(), volume.clone()]),
            serde_json::json!([AppResourceIdentity {
                kind: AppResourceKind::HostPath,
                ..volume.clone()
            }]),
        ] {
            let mut changed = record.clone();
            changed.checkpoint["builder_volumes"] = volumes;
            assert!(
                store
                    .resume_builder_restart_start(&changed, &target)
                    .await
                    .is_err()
            );
        }
        assert_eq!(
            store
                .get_compute_control(&app.app_id, &record.operation_id)
                .await
                .unwrap(),
            Some(record.clone()),
            "rejected continuation cannot mutate the original intent"
        );
        let (first, second) = tokio::join!(
            store.resume_builder_restart_start(&record, &target),
            store.resume_builder_restart_start(&record, &target),
        );
        let continued = match (first, second) {
            (Ok(record), Err(UserAppStoreError::VersionConflict))
            | (Err(UserAppStoreError::VersionConflict), Ok(record)) => record,
            result => panic!("one continuation CAS must win: {result:?}"),
        };
        assert_eq!(continued.operation_id, record.operation_id);
        assert_eq!(continued.request_id, record.request_id);
        assert_eq!(continued.generation, record.generation);
        assert_eq!(continued.lease, Some(lease));
        assert_eq!(
            continued.checkpoint["builder_volumes"],
            checkpoint["builder_volumes"]
        );
        assert_eq!(continued.stage, "starting");
        assert_eq!(continued.state, ComputeControlState::Running);
        assert!(continued.checkpoint["workload"].is_null());
        // A Stop supersedes the original intent; its old absence witness does
        // not authorize a later creator, even with the original receipt.
        store
            .admit_compute_control(&request(
                &app,
                &format!("stop{}", uuid::Uuid::new_v4().simple()),
                ComputeControlAction::Stop,
            ))
            .await
            .unwrap();
        assert!(
            store
                .resume_builder_restart_start(&continued, &target)
                .await
                .is_err()
        );
        let superseded = store
            .get_compute_control(&app.app_id, &continued.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(superseded.state, ComputeControlState::Superseded);
        let mut acknowledgement = ComputeControlDrainAcknowledgement {
            identity: identity.clone(),
            expected_revision: superseded.revision,
            evidence: ComputeControlDrainEvidence::ConditionalStartupFenced {
                checkpoint: superseded.checkpoint.clone(),
            },
        };
        assert!(
            store
                .acknowledge_compute_drain(&acknowledgement)
                .await
                .is_err(),
            "multi-step creation has no conditional single-write fence"
        );
        let mut changed = superseded.checkpoint.clone();
        changed["builder_volumes"][0]["uid"] = serde_json::json!("different-volume");
        acknowledgement.evidence = ComputeControlDrainEvidence::PersistedRuntimeAcknowledgement {
            checkpoint: changed,
        };
        assert!(
            store
                .acknowledge_compute_drain(&acknowledgement)
                .await
                .is_err()
        );
        acknowledgement.evidence = ComputeControlDrainEvidence::PersistedRuntimeAcknowledgement {
            checkpoint: superseded.checkpoint.clone(),
        };
        let drained = store
            .acknowledge_compute_drain(&acknowledgement)
            .await
            .unwrap();
        assert_eq!(drained.stage, "superseded_drained");
        assert_eq!(drained.checkpoint["previous"], superseded.checkpoint);
        assert_eq!(
            drained.lease, superseded.lease,
            "only exact runtime receipt release clears the lease"
        );
    }
    stopped_controller_removed_contract(store).await;
}

async fn stopped_controller_removed_contract(store: &ToastyUserAppStore) {
    for resume in [false, true] {
        let app = store
            .ensure_identity(&format!("removedbuilder{}", uuid::Uuid::new_v4().simple()))
            .await
            .unwrap();
        let pending = store
            .admit_compute_control(&request(
                &app,
                &format!("restart{}", uuid::Uuid::new_v4().simple()),
                ComputeControlAction::Restart,
            ))
            .await
            .unwrap();
        let identity = compute_identity(&pending);
        store
            .claim_compute_control(&identity, pending.revision)
            .await
            .unwrap();
        let mut record = store
            .bind_compute_lease(&identity, &compute_receipt())
            .await
            .unwrap();
        let old = BuilderControlTarget {
            context: record.execution_context().unwrap(),
            workload: Some(AppResourceIdentity {
                kind: AppResourceKind::StatefulSet,
                name: "previous-controller".into(),
                uid: "actual-old-controller-uid".into(),
                resource_version: Some("9".into()),
            }),
            pod: Some(BuilderPodIdentity {
                name: "previous-controller-0".into(),
                uid: "actual-old-pod-uid".into(),
                resource_version: "4".into(),
            }),
            resource_binding: None,
            restart_image: Some("frozen-runner-image".into()),
            restart_runtime_workspace: Some(paths::userapp_dev_workspace(&app.app_id)),
            builder_startup_mode: BuilderStartupMode::ManagementOnly,
        };
        let mut checkpoint = serde_json::to_value(&old).unwrap();
        checkpoint["builder_compute_single_write"] = serde_json::json!(true);
        checkpoint["builder_volumes"] = serde_json::json!([AppResourceIdentity {
            kind: AppResourceKind::PersistentVolumeClaim,
            name: "retained-workspace".into(),
            uid: "original-pvc-uid".into(),
            resource_version: None
        }]);
        let opaque = serde_json::json!({"future_archive": "retain-original-diagnostics"});
        checkpoint["builder_restart_template"] = opaque.clone();
        for stage in [ComputeControlStage::Stopping, ComputeControlStage::Stopped] {
            let mut progress = compute_progress(&record, stage);
            progress.checkpoint = checkpoint.clone();
            record = store.advance_compute_control(&progress).await.unwrap();
        }
        let mut absent = old.clone();
        absent.workload = None;
        absent.pod = None;
        absent.restart_runtime_workspace = None;
        let mut next = serde_json::to_value(&absent).unwrap();
        next["builder_volumes"] = checkpoint["builder_volumes"].clone();
        next["builder_restart_template_history"] =
            serde_json::json!([{"was_present":true,"value":opaque}]);
        let mut changed = compute_progress(&record, ComputeControlStage::Starting);
        changed.checkpoint = next.clone();
        changed.checkpoint["builder_volumes"][0]["uid"] = serde_json::json!("different-pvc");
        assert!(
            store.advance_compute_control(&changed).await.is_err(),
            "directory convergence cannot switch the retained volume"
        );
        let continued = if resume {
            store
                .resume_builder_restart_start(&record, &absent)
                .await
                .unwrap()
        } else {
            let mut progress = compute_progress(&record, ComputeControlStage::Starting);
            progress.checkpoint = next;
            store.advance_compute_control(&progress).await.unwrap()
        };
        assert_eq!(continued.operation_id, record.operation_id);
        assert_eq!(continued.lease, record.lease);
        assert_eq!(
            continued.checkpoint["builder_volumes"],
            checkpoint["builder_volumes"]
        );
        assert!(continued.checkpoint["workload"].is_null());
        assert!(
            !continued.has_conditional_compute_write(),
            "new multi-step creation is never an old controller's RV patch"
        );
        assert!(
            continued.checkpoint["builder_restart_template"] == opaque
                || continued.checkpoint["builder_restart_template_history"][0]["value"] == opaque,
            "opaque previous archive remains diagnostic"
        );

        // An unknown old Starting write has no stopped boundary. It cannot
        // become a new creator simply because observation saw an absent name.
        let app = store
            .ensure_identity(&format!("unknownbuilder{}", uuid::Uuid::new_v4().simple()))
            .await
            .unwrap();
        let pending = store
            .admit_compute_control(&request(
                &app,
                &format!("restart{}", uuid::Uuid::new_v4().simple()),
                ComputeControlAction::Restart,
            ))
            .await
            .unwrap();
        let identity = compute_identity(&pending);
        store
            .claim_compute_control(&identity, pending.revision)
            .await
            .unwrap();
        let mut record = store
            .bind_compute_lease(&identity, &compute_receipt())
            .await
            .unwrap();
        let mut original = old.clone();
        original.context = record.execution_context().unwrap();
        original.restart_runtime_workspace = Some(paths::userapp_dev_workspace(&app.app_id));
        let mut checkpoint = serde_json::to_value(&original).unwrap();
        checkpoint["builder_volumes"] = continued.checkpoint["builder_volumes"].clone();
        checkpoint["builder_compute_single_write"] = serde_json::json!(true);
        for stage in [
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Starting,
        ] {
            let mut progress = compute_progress(&record, stage);
            progress.checkpoint = checkpoint.clone();
            record = store.advance_compute_control(&progress).await.unwrap();
        }
        absent.context = original.context;
        assert!(
            store
                .resume_builder_restart_start(&record, &absent)
                .await
                .is_err()
        );
        let mut progress = compute_progress(&record, ComputeControlStage::Starting);
        progress.checkpoint = serde_json::to_value(&absent).unwrap();
        progress.checkpoint["builder_volumes"] = checkpoint["builder_volumes"].clone();
        assert!(store.advance_compute_control(&progress).await.is_err());
        assert_eq!(
            store
                .get_compute_control(&app.app_id, &record.operation_id)
                .await
                .unwrap(),
            Some(record)
        );
    }
}

#[tokio::test]
async fn absent_builder_restart_continuation_keeps_original_receipt_and_cas() {
    let root = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&root.path().join("absent-restart.db"))
        .await
        .unwrap();
    absent_builder_restart_continuation_contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL database"]
async fn absent_builder_restart_continuation_pg_keeps_original_receipt_and_cas() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        max_connections: Some(2),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    absent_builder_restart_continuation_contract(&store).await;
    store.shutdown().await.unwrap();
}

async fn observed_original_creation_finishes_atomically_contract(store: &ToastyUserAppStore) {
    for recovery_required in [false, true] {
        let app = store
            .ensure_identity(&format!(
                "observedcreation{}",
                uuid::Uuid::new_v4().simple()
            ))
            .await
            .unwrap();
        let pending = store
            .admit_compute_control(&request(
                &app,
                &format!("restart{}", uuid::Uuid::new_v4().simple()),
                ComputeControlAction::Restart,
            ))
            .await
            .unwrap();
        let identity = compute_identity(&pending);
        store
            .claim_compute_control(&identity, pending.revision)
            .await
            .unwrap();
        let mut original = store
            .bind_compute_lease(&identity, &compute_receipt())
            .await
            .unwrap();
        let target = BuilderControlTarget {
            context: original.execution_context().unwrap(),
            workload: None,
            pod: None,
            resource_binding: None,
            restart_image: Some("frozen-image".into()),
            restart_runtime_workspace: None,
            builder_startup_mode: BuilderStartupMode::ManagementOnly,
        };
        let volume = AppResourceIdentity {
            kind: AppResourceKind::PersistentVolumeClaim,
            name: "retained-workspace".into(),
            uid: "original-pvc".into(),
            resource_version: None,
        };
        let mut checkpoint = serde_json::to_value(&target).unwrap();
        checkpoint["builder_volumes"] = serde_json::json!([volume.clone()]);
        for stage in [
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Starting,
        ] {
            let mut progress = compute_progress(&original, stage);
            progress.checkpoint = checkpoint.clone();
            original = store.advance_compute_control(&progress).await.unwrap();
        }
        if recovery_required {
            let mut progress = compute_progress(&original, ComputeControlStage::Starting);
            progress.state = ComputeControlState::RecoveryRequired;
            progress.checkpoint = checkpoint.clone();
            progress.error_code = Some("LOST_RESPONSE".into());
            progress.error_message =
                Some("original creation finished after the caller disconnected".into());
            original = store.advance_compute_control(&progress).await.unwrap();
        }
        // Emulate an older ledger, after its original submitted stages. Strict
        // current executor progress is deliberately not used to add history.
        let operation = original.operation_id.clone();
        let mut historical = original.checkpoint.clone();
        historical["old_diagnostic"] = serde_json::json!({"opaque":"preserve"});
        historical["builder_restart_template_history"] =
            serde_json::json!({"legacy_archive":"opaque"});
        let payload = serde_json::to_string(&historical).unwrap();
        store
            .run(false, move |tx, backend| {
                let operation = operation.clone();
                let payload = payload.clone();
                Box::pin(async move {
                    toasty::sql::statement(crate::userapp_lifecycle::common::repo::sql(backend,
                    "UPDATE userapp_compute_controls SET checkpoint_json=$2 WHERE operation_id=$1"))
                    .bind(operation).bind(payload).exec(tx).await.map_err(crate::userapp_lifecycle::storage)?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        original = store
            .get_compute_control(&original.app_id, &original.operation_id)
            .await
            .unwrap()
            .unwrap();
        let mut observed = target.clone();
        observed.workload = Some(AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: "current-builder".into(),
            uid: "actual-sts".into(),
            resource_version: Some("10".into()),
        });
        observed.pod = Some(BuilderPodIdentity {
            name: "current-builder-0".into(),
            uid: "actual-pod".into(),
            resource_version: "11".into(),
        });
        for variant in 0..6 {
            let mut altered = observed.clone();
            let mut volumes = vec![volume.clone()];
            let mut snapshot = original.clone();
            match variant {
                0 => altered.context.executor_id = "foreign-executor".into(),
                1 => altered.restart_image = Some("new-intent".into()),
                2 => volumes[0].uid = "replacement-pvc".into(),
                3 => altered.builder_startup_mode = BuilderStartupMode::PreserveBusinessIntent,
                4 => snapshot.revision += 1,
                5 => altered.pod = None,
                _ => unreachable!(),
            }
            assert!(
                store
                    .finalize_observed_builder_creation(&snapshot, &altered, &volumes)
                    .await
                    .is_err()
            );
            assert_eq!(
                store
                    .get_compute_control(&original.app_id, &original.operation_id)
                    .await
                    .unwrap(),
                Some(original.clone())
            );
        }
        let (first, second) = tokio::join!(
            store.finalize_observed_builder_creation(
                &original,
                &observed,
                std::slice::from_ref(&volume)
            ),
            store.finalize_observed_builder_creation(
                &original,
                &observed,
                std::slice::from_ref(&volume)
            ),
        );
        let completed = match (first, second) {
            (Ok(r), Err(UserAppStoreError::VersionConflict))
            | (Err(UserAppStoreError::VersionConflict), Ok(r)) => r,
            other => panic!("one exact original completion transaction must win: {other:?}"),
        };
        assert_eq!(completed.state, ComputeControlState::Succeeded);
        assert_eq!(completed.operation_id, original.operation_id);
        assert_eq!(completed.executor_id, original.executor_id);
        assert_eq!(completed.generation, original.generation);
        assert_eq!(completed.request_id, original.request_id);
        assert_eq!(completed.request_fingerprint, original.request_fingerprint);
        assert_eq!(completed.lease, original.lease);
        assert_eq!(
            completed.checkpoint["old_diagnostic"],
            original.checkpoint["old_diagnostic"]
        );
        assert_eq!(
            completed.checkpoint["builder_volumes"],
            original.checkpoint["builder_volumes"]
        );
        assert_eq!(
            completed.checkpoint["builder_restart_template_history"],
            original.checkpoint["builder_restart_template_history"]
        );
        assert!(
            serde_json::from_value::<BuilderControlTarget>(completed.checkpoint.clone()).is_err()
        );
        let bound =
            BuilderControlTarget::from_observation_checkpoint(&completed.checkpoint).unwrap();
        assert_eq!(bound, observed);
    }
}

#[tokio::test]
async fn observed_builder_creation_completion_is_an_exact_original_atomic_transaction() {
    let root = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&root.path().join("observed-creation.db"))
        .await
        .unwrap();
    observed_original_creation_finishes_atomically_contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL database"]
async fn observed_builder_creation_completion_pg_is_an_exact_original_atomic_transaction() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        max_connections: Some(2),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    observed_original_creation_finishes_atomically_contract(&store).await;
    store.shutdown().await.unwrap();
}
