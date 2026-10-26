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
    stalled_restart_at_stages(
        state,
        name,
        &[ComputeControlStage::Stopping, ComputeControlStage::Stopped],
    )
    .await
}

/// The same claimed/bound fixture advanced through an explicit stage chain.
async fn stalled_restart_at_stages(
    state: &AppState,
    name: &str,
    stages: &[ComputeControlStage],
) -> ComputeControlRecord {
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
    let mut confirmed_checkpoint = checkpoint.clone();
    confirmed_checkpoint[KUBERNETES_RESTART_STOP_CONFIRMATION_KEY] = serde_json::json!({
        "evidence": "observed_stopped",
        "original_state": "running",
    });
    let mut record = bound;
    for stage in stages {
        let confirmed = matches!(
            stage,
            ComputeControlStage::Stopped
                | ComputeControlStage::Starting
                | ComputeControlStage::Verifying
        );
        record = state
            .userapp_store
            .advance_compute_control(&ComputeControlProgress {
                identity: identity.clone(),
                expected_revision: record.revision,
                state: ComputeControlState::Running,
                stage: *stage,
                checkpoint: if confirmed {
                    confirmed_checkpoint.clone()
                } else {
                    checkpoint.clone()
                },
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
    }
    record
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
    // The takeover's renewal heartbeat now holds the lease under the original
    // identity; the fixture verdict models that renewed state.
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::Held;
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
    // Pre-anchor the quiet window in the past: a first settle anchors "now"
    // and defers; the durable mark lets a later scan observe the window out.
    state
        .userapp_store
        .mark_compute_control_unheld(
            &record,
            chrono::Utc::now().timestamp_millis() - COMPUTE_SETTLEMENT_QUIET_MS - 1,
        )
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
    // Pre-anchor a stale quiet window: the held observation must drop it, so a
    // later unheld round re-anchors from its own observation and defers
    // instead of settling on the stale anchor.
    state
        .userapp_store
        .mark_compute_control_unheld(
            &record,
            chrono::Utc::now().timestamp_millis() - COMPUTE_SETTLEMENT_QUIET_MS - 1,
        )
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
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    settle_expired_compute_control(&state, &current)
        .await
        .unwrap();
    let reanchored = state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reanchored.state,
        ComputeControlState::Running,
        "the re-anchored quiet window defers settlement"
    );
}

/// The quiet window itself: expiry plus a first unheld observation defers;
/// only an anchor older than the window settles.
#[tokio::test]
async fn fresh_unheld_observation_waits_out_the_quiet_window() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    let record = stalled_stopped_restart(&state, "quietwindow").await;
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
    assert_eq!(
        current.state,
        ComputeControlState::Running,
        "a fresh unheld anchor is inside the quiet window"
    );
    settle_expired_compute_control(&state, &current)
        .await
        .unwrap();
    let still = state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.state, ComputeControlState::Running);
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
    state
        .userapp_store
        .mark_compute_control_unheld(
            &record,
            chrono::Utc::now().timestamp_millis() - COMPUTE_SETTLEMENT_QUIET_MS - 1,
        )
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

/// Fix 1 regression: the guard object alone never proves the lease is still
/// held — the renewal heartbeat can exit (identity change, conflicting
/// update) while the guard lives. validate_retained must re-observe the
/// physical lease through the runtime on every call.
#[tokio::test]
async fn takeover_guard_reobserves_the_physical_lease() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    *runtime.compute_inspection.lock().unwrap() = Some(ComputeLeaseInspection::Releasable(
        kubernetes_lease("deadexecutor"),
    ));
    let record = stalled_stopped_restart(&state, "guardreobserve").await;
    let takeover = take_over_expired_builder_lease(&state, &record)
        .await
        .unwrap()
        .expect("the handoff acquisition is a continuation authority");
    // The fixture verdict says the lease is not held: even though the guard
    // object is alive, retention must observe as false.
    assert!(!takeover.as_lease().validate_retained().await.unwrap());
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::Held;
    assert!(takeover.as_lease().validate_retained().await.unwrap());
}

/// Codex round-3 issue 1 regression: a double-false stop surface proves
/// nothing — scaled-to-zero-with-terminating-pods is exactly that shape — so
/// the settlement defers instead of claiming the workload still runs.
#[tokio::test]
async fn ambiguous_stop_surface_defers_settlement() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    let record =
        stalled_restart_at_stages(&state, "settlesurface", &[ComputeControlStage::Stopping]).await;
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    state
        .userapp_store
        .mark_compute_control_unheld(
            &record,
            chrono::Utc::now().timestamp_millis() - COMPUTE_SETTLEMENT_QUIET_MS - 1,
        )
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
    assert_eq!(
        current.state,
        ComputeControlState::Running,
        "a double-false stop surface (mid-teardown or unreadable) defers settlement"
    );
}

/// Codex round-3 issue 2 regression: a POSITIVE running observation — not a
/// double negative — settles the never-dispatched-stop shape (the stop receipt
/// query conflicts while replicas are non-zero; that error is classified,
/// never propagated).
#[tokio::test]
async fn running_stop_surface_settles_with_evidence() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    runtime
        .builder_running_observed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let record =
        stalled_restart_at_stages(&state, "runningstop", &[ComputeControlStage::Stopping]).await;
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    state
        .userapp_store
        .mark_compute_control_unheld(
            &record,
            chrono::Utc::now().timestamp_millis() - COMPUTE_SETTLEMENT_QUIET_MS - 1,
        )
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
    let message = settled.error_message.unwrap();
    assert!(
        message.contains("stop write can no longer land"),
        "the positive running observation backs the settlement: {message}"
    );
}

/// Codex round-3 issue 3 regression with corrected contract semantics:
/// Ok(None) from the retry preparation means the start COMMITTED and can only
/// be observed (the app81 form) — that is the settleable committed-not-ready
/// shape, recorded in the settlement diagnostic.
#[tokio::test]
async fn committed_start_surface_settles_as_never_ready() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    runtime
        .builder_retry_committed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let record = stalled_restart_at_stages(
        &state,
        "committedstart",
        &[
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Starting,
        ],
    )
    .await;
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    state
        .userapp_store
        .mark_compute_control_unheld(
            &record,
            chrono::Utc::now().timestamp_millis() - COMPUTE_SETTLEMENT_QUIET_MS - 1,
        )
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
    let message = settled.error_message.unwrap();
    assert!(
        message.contains("committed but never reached readiness"),
        "Ok(None) is the committed-not-ready evidence: {message}"
    );
}

/// The start side prefers normal completion and defers when the surface is
/// unclassifiable: with the fixture runtime unable to confirm the start
/// (reconcile None, retry unsupported) a starting-stage record is not failed
/// by the settlement.
#[tokio::test]
async fn starting_surface_without_evidence_defers_settlement() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    let record = stalled_restart_at_stages(
        &state,
        "startsurface",
        &[
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Starting,
        ],
    )
    .await;
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    state
        .userapp_store
        .mark_compute_control_unheld(
            &record,
            chrono::Utc::now().timestamp_millis() - COMPUTE_SETTLEMENT_QUIET_MS - 1,
        )
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
    assert_eq!(
        current.state,
        ComputeControlState::Running,
        "an unclassifiable start surface defers settlement to reconciliation-capable paths"
    );
}

/// Codex round-4 issue 2 wiring proof: a deadline settlement with a running
/// residue RELEASES the scope's admission fence — the next control admits
/// immediately (no OperationInProgress), and its own capture-fresh stop/start
/// phases take over physical reconciliation of the residue. The handoff is
/// the domain's established failed-with-residue pattern (fail_unrecoverable
/// image-roll: "围栏随终态自然释放，由业务 start 重建运行时").
#[tokio::test]
async fn settled_running_residue_hands_off_to_the_next_admission() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    runtime
        .builder_running_observed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // A REAL residue instance exists in the world: the successor's capture
    // must find it and act on its exact identity.
    *runtime.builder_workload.lock().unwrap() = Some("rcoder-app-builder-handoff".into());
    let record =
        stalled_restart_at_stages(&state, "handoff", &[ComputeControlStage::Stopping]).await;
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    state
        .userapp_store
        .mark_compute_control_unheld(
            &record,
            chrono::Utc::now().timestamp_millis() - COMPUTE_SETTLEMENT_QUIET_MS - 1,
        )
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
    // The fence is released: the scope admits a fresh control whose
    // capture-fresh phases now own the physical residue.
    let admitted = state
        .userapp_store
        .admit_compute_control(&ComputeControlRequest {
            builder_startup_mode: Default::default(),
            app_id: settled.app_id.clone(),
            lifecycle_id: settled.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: "handoffsuccessor".into(),
            request_id: "handoffsuccessor-request".into(),
            request_fingerprint: "b".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
        })
        .await
        .unwrap();
    assert_eq!(admitted.state, ComputeControlState::Pending);
    // The handoff is physical, not administrative: the successor EXECUTES to
    // a terminal state against the same world, its capture-fresh stop phase
    // owning whatever residue the settled predecessor left.
    let successor = admitted.clone();
    execute_pending(&state, admitted).await.unwrap();
    let finished = state
        .userapp_store
        .get_compute_control(&successor.app_id, &successor.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        finished.state,
        ComputeControlState::Succeeded,
        "the successor control executes to completion after the settlement"
    );
    // The physical handoff: the successor captured the residue workload and
    // issued exactly one stop against its EXACT captured identity.
    let calls = runtime.builder_control_calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![(
            "builder-uid".into(),
            "rcoder-app-builder-handoff".into(),
            false
        )],
        "the successor's stop acts on the residue's exact captured uid"
    );
}

/// Codex round-5 issue 1 regression: the receipt-free running observation
/// (desired replicas + owned pod — a Pending or CrashLooping shape) must NOT
/// gate the start side; only receipt-confirmed readiness defers to the
/// completion branch. Otherwise the committed-but-never-ready form (app81)
/// would be re-blocked forever.
#[tokio::test]
async fn never_ready_start_surface_is_not_blocked_by_running_observation() {
    let (state, runtime) = test_state().await;
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::NotHeld;
    // Desired replicas with an owned pod that never becomes ready.
    runtime
        .builder_running_observed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // The start is committed and can only be observed (the app81 evidence).
    runtime
        .builder_retry_committed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let record = stalled_restart_at_stages(
        &state,
        "neverready",
        &[
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Starting,
        ],
    )
    .await;
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    state
        .userapp_store
        .mark_compute_control_unheld(
            &record,
            chrono::Utc::now().timestamp_millis() - COMPUTE_SETTLEMENT_QUIET_MS - 1,
        )
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
    assert_eq!(
        settled.state,
        ComputeControlState::Failed,
        "a never-ready committed start settles instead of being deferred forever"
    );
    let message = settled.error_message.unwrap();
    assert!(
        message.contains("committed but never reached readiness"),
        "{message}"
    );
}
