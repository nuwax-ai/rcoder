//! Deadline settlement and expired-lease takeover contracts. The runtime
//! polarity (receipt verdicts, handoff outcomes) is driven by FenceRuntime;
//! durable expiry is enforced inside the storage transaction.
use super::*;
use crate::userapp_builder::creation::fence_settler_tests::{
    FenceRuntime, ReceiptVerdict, runtime_state,
};
use std::sync::Arc;

fn kubernetes_lease(executor_id: &str) -> UserAppOperationLeaseReceipt {
    UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "rcoder-builder".into(),
        name: "rcoder-app-builder-deadlinefixture".into(),
        uid: "e".repeat(36),
        resource_version: "12".into(),
        token: executor_id.into(),
    }
}

/// A Dev Restart claimed by a dead executor, bound to a Kubernetes lease and
/// stalled at the durable stopped boundary with a confirmation witness.
async fn stalled_stopped_restart(state: &AppState, name: &str) -> ComputeControlRecord {
    let app = state.userapp_store.ensure_identity(name).await.unwrap();
    let pending = state
        .userapp_store
        .admit_compute_control(&ComputeControlRequest {
            builder_startup_mode: Default::default(),
            app_id: name.into(),
            lifecycle_id: app.lifecycle_id,
            scope: UserAppOperationScope::Dev,
            request_id: format!("request{name}"),
            request_fingerprint: "a".repeat(64),
            operation_id: format!("restart{name}"),
            action: ComputeControlAction::Restart,
            restart_image_roll: false,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: name.into(),
        lifecycle_id: pending.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "deadexecutor".into(),
    };
    state
        .userapp_store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let bound = state
        .userapp_store
        .bind_compute_lease(&identity, &kubernetes_lease(&identity.executor_id))
        .await
        .unwrap();
    let checkpoint = serde_json::json!({
        "context": {
            "app_id": name,
            "lifecycle_id": bound.lifecycle_id,
            "operation_id": bound.operation_id,
            "executor_id": identity.executor_id,
            "request_fingerprint": bound.request_fingerprint,
        },
        "workload": {
            "kind": "StatefulSet",
            "name": format!("rcoder-app-builder-{name}"),
            "uid": "f".repeat(36),
            "resource_version": "42",
        },
        "builder_startup_mode": "management_only",
    });
    let stopping = state
        .userapp_store
        .advance_compute_control(&ComputeControlProgress {
            identity: identity.clone(),
            expected_revision: bound.revision,
            state: ComputeControlState::Running,
            stage: ComputeControlStage::Stopping,
            checkpoint: checkpoint.clone(),
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    let mut confirmed_checkpoint = checkpoint;
    confirmed_checkpoint[KUBERNETES_RESTART_STOP_CONFIRMATION_KEY] = serde_json::json!({
        "evidence": "observed_stopped",
        "original_state": "running",
    });
    state
        .userapp_store
        .advance_compute_control(&ComputeControlProgress {
            identity,
            expected_revision: stopping.revision,
            state: ComputeControlState::Running,
            stage: ComputeControlStage::Stopped,
            checkpoint: confirmed_checkpoint,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap()
}

async fn test_state() -> (Arc<AppState>, Arc<FenceRuntime>) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("deadline.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    let state = runtime_state(runtime.clone(), store, None).await;
    (state, runtime)
}

/// 修复前反例（第四断点）：过期租约 + 持久化停止边界确认时，接管协议以原身份
/// 续接。修复前 `take_over_expired_builder_lease` 不存在，续接在
/// `finalize_confirmed` 被拒后卡在 stopped。
#[tokio::test]
async fn expired_kubernetes_lease_is_taken_over_under_original_identity() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    *runtime.compute_inspection.lock().unwrap() = Some(ComputeLeaseInspection::Releasable(
        kubernetes_lease("deadexecutor"),
    ));
    let record = stalled_stopped_restart(&state, "takeover").await;
    let takeover = take_over_expired_builder_lease(&state, &record)
        .await
        .unwrap();
    let takeover = takeover.expect("the handoff acquisition is a continuation authority");
    let guard = takeover.as_lease();
    assert_eq!(guard.receipt().as_ref(), record.lease.as_ref());
    assert!(guard.validate_retained().await.unwrap());
}

/// A lease that is held or renewed again is never taken over; the record waits
/// for its existing recovery paths.
#[tokio::test]
async fn held_lease_is_not_taken_over() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    *runtime.compute_inspection.lock().unwrap() = Some(ComputeLeaseInspection::Held);
    let record = stalled_stopped_restart(&state, "heldlease").await;
    assert!(
        take_over_expired_builder_lease(&state, &record)
            .await
            .unwrap()
            .is_none()
    );
}

/// The Docker lease domain keeps its own retained-fd re-acquisition path. The
/// inspection is set to a shape the runtime would acquire, so a missing domain
/// gate would surface as `Some(..)` and fail this contract.
#[tokio::test]
async fn docker_receipt_never_enters_the_kubernetes_takeover() {
    let (state, _runtime) = test_state().await;
    let mut record = stalled_stopped_restart(&state, "dockerdomain").await;
    record.lease = Some(UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: 1,
        inode: 1,
        token: record.execution_context().unwrap().executor_id,
    });
    assert!(
        take_over_expired_builder_lease(&state, &record)
            .await
            .unwrap()
            .is_none()
    );
}

/// A deadline-expired operation is never revived by a takeover; the settlement
/// scan owns its terminal failure.
#[tokio::test]
async fn deadline_expired_operation_is_not_revived_by_takeover() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    *runtime.compute_inspection.lock().unwrap() = Some(ComputeLeaseInspection::Releasable(
        kubernetes_lease("deadexecutor"),
    ));
    let record = stalled_stopped_restart(&state, "expiredtakeover").await;
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    assert!(
        take_over_expired_builder_lease(&state, &record)
            .await
            .unwrap()
            .is_none()
    );
}

/// 修复前反例（app81 starting 形态）：无 deadline 机制时非终态控制永久阻塞
/// scope 准入。到期 + 租约不再续期 → 结算为 Failed 终态，释放准入围栏。
#[tokio::test]
async fn expired_deadline_settles_unheld_control_as_failed() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    let record = stalled_stopped_restart(&state, "settle").await;
    // Bind an already-expired deadline directly, as an operation that outran
    // its budget durably has; the scan observation then reports expiry.
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    assert!(compute_deadline_expired(&state, &record).await);
    settle_expired_compute_control(&state, &record)
        .await
        .unwrap();
    let settled = state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settled.state, ComputeControlState::Failed);
    assert_eq!(
        settled.stage, record.stage,
        "the last durable stage is kept"
    );
    assert_eq!(settled.error_code.as_deref(), Some("ERR_DEADLINE_EXCEEDED"));
    assert_eq!(settled.lease, record.lease, "the receipt stays for cleanup");
    assert_eq!(settled.checkpoint, record.checkpoint);
}

/// A control without any bound deadline gets its budget from the first scan
/// observation (never from its creation age) and is not expired by it.
#[tokio::test]
async fn missing_deadline_is_bound_by_the_first_scan_observation() {
    let (state, _runtime) = test_state().await;
    let record = stalled_stopped_restart(&state, "lazybind").await;
    assert!(!compute_deadline_expired(&state, &record).await);
    let bound = state
        .userapp_store
        .compute_control_deadline(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .expect("the scan observation bound the budget");
    assert!(bound > chrono::Utc::now().timestamp_millis());
    assert!(!compute_deadline_expired(&state, &record).await);
}

/// 复核点 A：租约仍在续期（原执行者或接管续接中的副本存活）时，到期不结算。
#[tokio::test]
async fn held_lease_defers_deadline_settlement() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::Held;
    let record = stalled_stopped_restart(&state, "heldsettle").await;
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    settle_expired_compute_control(&state, &record)
        .await
        .unwrap();
    let current = state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.state, ComputeControlState::Running);
}

/// An unconfirmable lease defers settlement to a later scan instead of
/// settling blind.
#[tokio::test]
async fn unconfirmable_lease_defers_deadline_settlement() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::TakenOver;
    let record = stalled_stopped_restart(&state, "conflictsettle").await;
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    settle_expired_compute_control(&state, &record)
        .await
        .unwrap();
    let current = state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.state, ComputeControlState::Running);
}

/// Terminal records are never settled again; the settlement of an expired
/// record is a one-shot terminal CAS.
#[tokio::test]
async fn terminal_record_is_never_settled() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    let record = stalled_stopped_restart(&state, "terminal").await;
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    settle_expired_compute_control(&state, &record)
        .await
        .unwrap();
    let settled = state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settled.state, ComputeControlState::Failed);
    // Settling the stale snapshot again is a benign no-op.
    settle_expired_compute_control(&state, &record)
        .await
        .unwrap();
    let again = state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.revision, settled.revision);
}
