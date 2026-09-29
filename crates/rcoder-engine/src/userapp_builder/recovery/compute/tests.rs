use super::*;
use crate::userapp_builder::creation::fence_settler_tests::{
    FenceRuntime, ReceiptVerdict, runtime_state,
};
use shared_types::*;
use std::sync::{Arc, atomic::Ordering};

async fn seed(state: &AppState, name: &str, bound: bool, unknown: bool) -> ComputeControlRecord {
    let app = state.userapp_store.ensure_identity(name).await.unwrap();
    let pending = state
        .userapp_store
        .admit_compute_control(&ComputeControlRequest {
            app_id: name.into(),
            lifecycle_id: app.lifecycle_id,
            scope: UserAppOperationScope::Dev,
            request_id: format!("request{name}"),
            request_fingerprint: "a".repeat(64),
            operation_id: format!("stop{name}"),
            action: ComputeControlAction::Stop,
            restart_image_roll: true,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: name.into(),
        lifecycle_id: pending.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "lostexecutor".into(),
    };
    let mut record = state
        .userapp_store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    if bound {
        record = state
            .userapp_store
            .bind_compute_lease(
                &identity,
                &UserAppOperationLeaseReceipt::Docker {
                    service_type: ServiceType::UserappBuilder,
                    device: 1,
                    inode: 1,
                    token: identity.executor_id.clone(),
                },
            )
            .await
            .unwrap();
    }
    if unknown {
        record = state
            .userapp_store
            .advance_compute_control(&ComputeControlProgress {
                identity,
                expected_revision: record.revision,
                state: ComputeControlState::RecoveryRequired,
                stage: ComputeControlStage::DrainingPrevious,
                checkpoint: serde_json::json!({"restart_image_roll":true,"unknown_write":true}),
                error_code: Some("UNKNOWN".into()),
                error_message: Some("extra evidence".into()),
            })
            .await
            .unwrap();
    }
    record
}

#[tokio::test]
async fn scanner_recovers_initial_checkpoint_and_bound_lease_but_preserves_unknown_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("scan.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    runtime
        .compute_release_succeeds
        .store(true, Ordering::SeqCst);
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let mut recovery = seed(&state, "recoveryscan", false, false).await;
    recovery = store
        .advance_compute_control(&ComputeControlProgress {
            identity: ComputeExecutorIdentity {
                app_id: recovery.app_id.clone(),
                lifecycle_id: recovery.lifecycle_id.clone(),
                scope: recovery.scope,
                operation_id: recovery.operation_id.clone(),
                generation: recovery.generation,
                executor_id: recovery.executor_id.clone().unwrap(),
            },
            expected_revision: recovery.revision,
            state: ComputeControlState::RecoveryRequired,
            stage: ComputeControlStage::DrainingPrevious,
            checkpoint: recovery.checkpoint.clone(),
            error_code: Some("OLD_FAILURE".into()),
            error_message: Some("interrupted before lease acquisition".into()),
        })
        .await
        .unwrap();
    let records = [
        seed(&state, "unboundscan", false, false).await,
        seed(&state, "boundscan", true, false).await,
        recovery,
        seed(&state, "unknownscan", false, true).await,
    ];
    let mut tasks = RecoveryTasks::default();
    discover_pending(&state, &mut tasks, &mut None)
        .await
        .unwrap();
    assert_eq!(
        tasks.active.len(),
        3,
        "unknown checkpoints never enter early recovery"
    );
    while let Some((_, result)) = tasks.next().await {
        result.unwrap();
    }
    for original in &records[..3] {
        let current = store
            .get_compute_control(&original.app_id, &original.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.state, ComputeControlState::Succeeded);
        assert_eq!(current.generation, original.generation);
        assert_eq!(current.request_id, original.request_id);
        assert_eq!(current.lifecycle_id, original.lifecycle_id);
        assert_ne!(current.executor_id, original.executor_id);
        assert!(current.lease.is_none());
        store
            .check_compute_access(&current.app_id, current.scope, true)
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .get_compute_control(&records[3].app_id, &records[3].operation_id)
            .await
            .unwrap(),
        Some(records[3].clone())
    );
    assert_eq!(runtime.compute_prepares.load(Ordering::SeqCst), 3);
    assert_eq!(runtime.compute_releases.load(Ordering::SeqCst), 1);
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn early_recovery_retains_held_foreign_and_unobserved_receipts_then_retries_original() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("inspection.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    runtime
        .compute_release_succeeds
        .store(true, Ordering::SeqCst);
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let original = seed(&state, "inspection", true, false).await;
    let mut snapshot = original.clone();
    for (verdict, transport, code) in [
        (ComputeLeaseInspection::Held, false, "COMPUTE_LEASE_HELD"),
        (
            ComputeLeaseInspection::IdentityChanged("foreign receipt".into()),
            false,
            "COMPUTE_LEASE_IDENTITY_CHANGED",
        ),
        (
            ComputeLeaseInspection::Absent,
            true,
            "COMPUTE_OBSERVATION_FAILED",
        ),
    ] {
        *runtime.compute_inspection.lock().unwrap() = Some(verdict);
        *runtime.receipt_verdict.lock().unwrap() = if transport {
            ReceiptVerdict::Transport
        } else {
            ReceiptVerdict::NotHeld
        };
        assert!(
            crate::userapp_builder::compute_drain_recovery::prepare_resume(&state, &snapshot)
                .await
                .unwrap()
                .is_none()
        );
        snapshot = store
            .get_compute_control(&original.app_id, &original.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.state, ComputeControlState::RecoveryRequired);
        assert_eq!(snapshot.error_code.as_deref(), Some(code));
        assert_eq!(snapshot.lease, original.lease);
        assert_eq!(snapshot.executor_id, original.executor_id);
        assert_eq!(runtime.compute_prepares.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.compute_releases.load(Ordering::SeqCst), 0);
    }
    runtime.compute_stall.store(true, Ordering::SeqCst);
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    assert!(
        crate::userapp_builder::compute_drain_recovery::prepare_resume_with_budget(
            &state,
            &snapshot,
            Duration::from_millis(20)
        )
        .await
        .unwrap()
        .is_none()
    );
    snapshot = store
        .get_compute_control(&original.app_id, &original.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        snapshot
            .error_message
            .as_deref()
            .unwrap()
            .contains("timed out")
    );
    assert_eq!(snapshot.lease, original.lease);
    runtime.compute_stall.store(false, Ordering::SeqCst);
    *runtime.compute_inspection.lock().unwrap() = None;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    let view = crate::userapp_builder::compute_control::recover(
        &state,
        &snapshot.app_id,
        &snapshot.operation_id,
        snapshot.revision,
    )
    .await
    .unwrap();
    assert_eq!(view.operation_id, original.operation_id);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = store
                .get_compute_control(&original.app_id, &original.operation_id)
                .await
                .unwrap()
                .unwrap();
            if current.state == ComputeControlState::Succeeded && current.lease.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("explicit recovery completes original Stop");
    store.shutdown().await.unwrap();
}
