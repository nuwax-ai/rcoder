use super::*;
use crate::userapp_builder::creation::fence_settler_tests::{
    FenceRuntime, ReceiptVerdict, runtime_state,
};
use shared_types::*;
use std::sync::{Arc, atomic::Ordering};

mod handoff_attempt_tests;
mod previous_owner_tests;

async fn seed(state: &AppState, name: &str, bound: bool, unknown: bool) -> ComputeControlRecord {
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
    let seeded = seed(&state, "inspection", true, false).await;
    // This case exercises an already-fenced, failed owner. A normal Running
    // holder is instead preserved by the real-FD concurrency regressions.
    let original = store
        .advance_compute_control(&ComputeControlProgress {
            identity: ComputeExecutorIdentity {
                app_id: seeded.app_id.clone(),
                lifecycle_id: seeded.lifecycle_id.clone(),
                scope: seeded.scope,
                operation_id: seeded.operation_id.clone(),
                generation: seeded.generation,
                executor_id: seeded.executor_id.clone().unwrap(),
            },
            expected_revision: seeded.revision,
            state: ComputeControlState::RecoveryRequired,
            stage: ComputeControlStage::DrainingPrevious,
            checkpoint: seeded.checkpoint.clone(),
            error_code: Some("INTERRUPTED_PREPARE".into()),
            error_message: Some("Original owner failed before binding completed".into()),
        })
        .await
        .unwrap();
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

mod restart_stop_tests;

/// 修复前反例（F2 自查发现的饥饿缺陷）：扫描的 deadline 结算分支曾以
/// `continue` 独占调度——deadline 已过但租约仍被续期（执行者合法超预算或
/// 接管续接中）的记录，其 compute-complete 等工作分支被永久跳过，本可
/// Succeeded 的记录最终只能被 Failed 收口。修复后结算作为并发观察者与
/// 工作分支并存：本用例断言两个任务同时派发且记录经完成分支 Succeeded。
#[tokio::test]
async fn expired_deadline_with_held_lease_still_dispatches_completion_branches() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("deadline-scan.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    // Held: the physical lease is still renewed, so the settlement defers.
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::Held;
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let app = state
        .userapp_store
        .ensure_identity("deadlineheld")
        .await
        .unwrap();
    let pending = state
        .userapp_store
        .admit_compute_control(&ComputeControlRequest {
            builder_startup_mode: Default::default(),
            app_id: "deadlineheld".into(),
            lifecycle_id: app.lifecycle_id,
            scope: UserAppOperationScope::Dev,
            request_id: "requestdeadlineheld".into(),
            request_fingerprint: "a".repeat(64),
            operation_id: "restartdeadlineheld".into(),
            action: ComputeControlAction::Restart,
            restart_image_roll: false,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: "deadlineheld".into(),
        lifecycle_id: pending.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "stuckexecutor".into(),
    };
    state
        .userapp_store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let lease = UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "rcoder-builder".into(),
        name: "rcoder-app-builder-deadlineheld".into(),
        uid: "b".repeat(36),
        resource_version: "3".into(),
        token: identity.executor_id.clone(),
    };
    let mut record = state
        .userapp_store
        .bind_compute_lease(&identity, &lease)
        .await
        .unwrap();
    for stage in [
        ComputeControlStage::Stopping,
        ComputeControlStage::Stopped,
        ComputeControlStage::Starting,
        ComputeControlStage::Verifying,
    ] {
        record = state
            .userapp_store
            .advance_compute_control(&ComputeControlProgress {
                identity: identity.clone(),
                expected_revision: record.revision,
                state: ComputeControlState::Running,
                stage,
                checkpoint: serde_json::json!({"witness": true}),
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
    }
    // The operation's total deadline has long expired while its lease is held.
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    let mut tasks = RecoveryTasks::default();
    discover_pending(&state, &mut tasks, &mut None)
        .await
        .unwrap();
    assert_eq!(
        tasks.active.len(),
        2,
        "the settlement observer and the completion branch must both be dispatched"
    );
    while let Some((_, result)) = tasks.next().await {
        result.unwrap();
    }
    let current = store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current.state,
        ComputeControlState::Succeeded,
        "the held-lease record completes through its normal branch, never starved into failure"
    );
    store.shutdown().await.unwrap();
}

/// 修复前反例（Codex 指出的满载边界）：只剩一个恢复槽位时，deadline 结算
/// 任务若先占位，正常续接分支的 push 会静默失败，饱和扫描下记录被反复
/// 饿死。修复后结算派发预留一个槽位：单槽剩余时结算让位，完成分支照常
/// 派发。
#[tokio::test]
async fn last_recovery_slot_goes_to_work_branches_not_the_deadline_observer() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("last-slot.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(FenceRuntime::default());
    // Held keeps the settlement deferring, so the observer cannot finish the
    // record itself; only the completion branch can.
    *runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::Held;
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    let app = state
        .userapp_store
        .ensure_identity("lastslot")
        .await
        .unwrap();
    let pending = state
        .userapp_store
        .admit_compute_control(&ComputeControlRequest {
            builder_startup_mode: Default::default(),
            app_id: "lastslot".into(),
            lifecycle_id: app.lifecycle_id,
            scope: UserAppOperationScope::Dev,
            request_id: "requestlastslot".into(),
            request_fingerprint: "a".repeat(64),
            operation_id: "restartlastslot".into(),
            action: ComputeControlAction::Restart,
            restart_image_roll: false,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: "lastslot".into(),
        lifecycle_id: pending.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "lastslotexecutor".into(),
    };
    state
        .userapp_store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let lease = UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "rcoder-builder".into(),
        name: "rcoder-app-builder-lastslot".into(),
        uid: "c".repeat(36),
        resource_version: "5".into(),
        token: identity.executor_id.clone(),
    };
    let mut record = state
        .userapp_store
        .bind_compute_lease(&identity, &lease)
        .await
        .unwrap();
    for stage in [
        ComputeControlStage::Stopping,
        ComputeControlStage::Stopped,
        ComputeControlStage::Starting,
        ComputeControlStage::Verifying,
    ] {
        record = state
            .userapp_store
            .advance_compute_control(&ComputeControlProgress {
                identity: identity.clone(),
                expected_revision: record.revision,
                state: ComputeControlState::Running,
                stage,
                checkpoint: serde_json::json!({"witness": true}),
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
    }
    state
        .userapp_store
        .bind_compute_control_deadline(&record, chrono::Utc::now().timestamp_millis() - 1)
        .await
        .unwrap();
    // Saturate all but one recovery slot with never-resolving tasks.
    let mut tasks = RecoveryTasks::default();
    for slot in 0..7 {
        assert!(tasks.push(
            format!("saturation-{slot}"),
            std::future::pending::<anyhow::Result<()>>()
        ));
    }
    discover_pending(&state, &mut tasks, &mut None)
        .await
        .unwrap();
    assert_eq!(
        tasks.active.len(),
        8,
        "exactly the completion branch took the last slot"
    );
    assert!(
        !tasks
            .active
            .iter()
            .any(|name| name.starts_with("compute-deadline:")),
        "the deadline observer reserved the last slot for real work"
    );
    store.shutdown().await.unwrap();
}
