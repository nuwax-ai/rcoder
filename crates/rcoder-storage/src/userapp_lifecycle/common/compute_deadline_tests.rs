//! Compute control deadline contracts: idempotent binding that never extends
//! an operation's budget, and expiry settlement whose durable re-verification
//! refuses unexpired, missing or stale-authority settlements.
use super::ToastyUserAppStore;
use shared_types::*;

async fn claimed_control(
    store: &ToastyUserAppStore,
    name: &str,
    action: ComputeControlAction,
) -> ComputeControlRecord {
    let app = store.ensure_identity(name).await.unwrap();
    let id = uuid::Uuid::new_v4().simple().to_string();
    let pending = store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: id.clone(),
            request_id: id,
            request_fingerprint: "a".repeat(64),
            action,
            restart_image_roll: false,
            builder_startup_mode: BuilderStartupMode::ManagementOnly,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: pending.app_id.clone(),
        lifecycle_id: pending.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "deadlineexecutor".into(),
    };
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap()
}

async fn contract(store: &ToastyUserAppStore) {
    let now = chrono::Utc::now().timestamp_millis();
    // The PostgreSQL contract runs on a persistent disposable database; a
    // per-run suffix keeps leftover non-terminal controls from a previous
    // run from fencing this run's admissions.
    let run = uuid::Uuid::new_v4().simple().to_string();
    let n = |name: &str| format!("{name}{run}");

    // Binding is idempotent and never extends an operation's budget: the
    // second, larger bind returns the first row unchanged.
    let bound = claimed_control(store, &n("deadlinebind"), ComputeControlAction::Restart).await;
    let first = store
        .bind_compute_control_deadline(&bound, now + 60_000)
        .await
        .unwrap();
    assert_eq!(first, now + 60_000);
    let again = store
        .bind_compute_control_deadline(&bound, now + 600_000)
        .await
        .unwrap();
    assert_eq!(again, first);

    // Non-positive deadlines are rejected outright.
    assert!(matches!(
        store.bind_compute_control_deadline(&bound, 0).await,
        Err(UserAppStoreError::InvalidOperation(_))
    ));

    // A terminal record refuses binding (no budget for finished work).
    let terminal = claimed_control(store, &n("deadlineterminal"), ComputeControlAction::Stop).await;
    let identity = ComputeExecutorIdentity {
        app_id: terminal.app_id.clone(),
        lifecycle_id: terminal.lifecycle_id.clone(),
        scope: terminal.scope,
        operation_id: terminal.operation_id.clone(),
        generation: terminal.generation,
        executor_id: terminal.executor_id.clone().unwrap(),
    };
    let failed = store
        .advance_compute_control(&ComputeControlProgress {
            identity,
            expected_revision: terminal.revision,
            state: ComputeControlState::Failed,
            stage: ComputeControlStage::DrainingPrevious,
            checkpoint: serde_json::Value::Null,
            error_code: Some("ERR_BACKEND_ERROR".into()),
            error_message: Some("fixture failure".into()),
        })
        .await
        .unwrap();
    assert!(matches!(
        store
            .bind_compute_control_deadline(&failed, now + 60_000)
            .await,
        Err(UserAppStoreError::VersionConflict)
    ));

    // Settlement: an expired deadline fails the non-terminal control while
    // preserving its last durable stage, checkpoint and lease for diagnosis.
    let mut expired =
        claimed_control(store, &n("deadlinesettle"), ComputeControlAction::Restart).await;
    let lease = UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "rcoder-builder".into(),
        name: "rcoder-app-builder-deadline".into(),
        uid: "a".repeat(36),
        resource_version: "7".into(),
        token: expired.executor_id.clone().unwrap(),
    };
    let identity = ComputeExecutorIdentity {
        app_id: expired.app_id.clone(),
        lifecycle_id: expired.lifecycle_id.clone(),
        scope: expired.scope,
        operation_id: expired.operation_id.clone(),
        generation: expired.generation,
        executor_id: expired.executor_id.clone().unwrap(),
    };
    expired = store.bind_compute_lease(&identity, &lease).await.unwrap();
    expired = store
        .advance_compute_control(&ComputeControlProgress {
            identity: identity.clone(),
            expected_revision: expired.revision,
            state: ComputeControlState::RecoveryRequired,
            stage: ComputeControlStage::Stopping,
            checkpoint: serde_json::json!({"deadline-witness": true}),
            error_code: Some("ERR_STOP_REPLY_TIMEOUT".into()),
            error_message: Some("original scale-down response timed out".into()),
        })
        .await
        .unwrap();
    store
        .bind_compute_control_deadline(&expired, now - 1)
        .await
        .unwrap();

    // Without a deadline (or before expiry) settlement is refused.
    let unexpired = claimed_control(
        store,
        &n("deadlineunexpired"),
        ComputeControlAction::Restart,
    )
    .await;
    assert!(matches!(
        store
            .fail_expired_compute_control(&unexpired, "ERR_DEADLINE_EXCEEDED", "not expired yet", 0)
            .await,
        Err(UserAppStoreError::InvalidOperation(_))
    ));

    // A settler without its anchor measured (the anchor row is still NULL)
    // never settles: expiry alone is not settlement authority.
    assert!(matches!(
        store
            .fail_expired_compute_control(
                &expired,
                "ERR_DEADLINE_EXCEEDED",
                "no anchored quiet window",
                0
            )
            .await,
        Err(UserAppStoreError::VersionConflict)
    ));
    let anchor = now - 400_000;
    store
        .mark_compute_control_unheld(&expired, anchor)
        .await
        .unwrap();

    let settled = store
        .fail_expired_compute_control(
            &expired,
            "ERR_DEADLINE_EXCEEDED",
            "exceeded its total deadline",
            anchor,
        )
        .await
        .unwrap();
    assert_eq!(settled.state, ComputeControlState::Failed);
    assert_eq!(settled.stage, "stopping", "the last durable stage is kept");
    assert_eq!(settled.lease, Some(lease), "the receipt stays for cleanup");
    assert_eq!(settled.checkpoint, expired.checkpoint);
    assert_eq!(settled.error_code.as_deref(), Some("ERR_DEADLINE_EXCEEDED"));
    assert_eq!(
        settled.error_message.as_deref(),
        Some("exceeded its total deadline")
    );
    assert_eq!(settled.revision, expired.revision + 1);

    // The quiet-window anchor: idempotent, never sliding, cleared on a held
    // observation, and requiring a bound deadline.
    let anchored =
        claimed_control(store, &n("deadlineanchor"), ComputeControlAction::Restart).await;
    assert!(matches!(
        store
            .mark_compute_control_unheld(&anchored, now - 1000)
            .await,
        Err(UserAppStoreError::InvalidOperation(_))
    ));
    store
        .bind_compute_control_deadline(&anchored, now + 60_000)
        .await
        .unwrap();
    assert_eq!(
        store
            .mark_compute_control_unheld(&anchored, now - 1000)
            .await
            .unwrap(),
        now - 1000
    );
    // A later observation never slides the anchored window.
    assert_eq!(
        store
            .mark_compute_control_unheld(&anchored, now - 500)
            .await
            .unwrap(),
        now - 1000
    );
    // Held again drops the anchor; the next unheld observation re-anchors.
    store.clear_compute_control_unheld(&anchored).await.unwrap();
    store.clear_compute_control_unheld(&anchored).await.unwrap();
    assert_eq!(
        store
            .mark_compute_control_unheld(&anchored, now - 200)
            .await
            .unwrap(),
        now - 200
    );

    // A stale snapshot (the record moved on) never settles twice.
    assert!(matches!(
        store
            .fail_expired_compute_control(
                &expired,
                "ERR_DEADLINE_EXCEEDED",
                "stale settler replay",
                anchor
            )
            .await,
        Err(UserAppStoreError::VersionConflict)
    ));

    // The Codex-reproduced clear/settle race on a fresh record: the settler
    // measured anchor T0, a concurrent held observation cleared it, so the
    // settlement transaction must refuse (pre-fix this returned Ok(Failed)).
    let raced = claimed_control(store, &n("deadlinerace"), ComputeControlAction::Restart).await;
    store
        .bind_compute_control_deadline(&raced, now - 1)
        .await
        .unwrap();
    let t0 = now - 500_000;
    store.mark_compute_control_unheld(&raced, t0).await.unwrap();
    store.clear_compute_control_unheld(&raced).await.unwrap();
    assert!(matches!(
        store
            .fail_expired_compute_control(
                &raced,
                "ERR_DEADLINE_EXCEEDED",
                "settler racing a clear",
                t0
            )
            .await,
        Err(UserAppStoreError::VersionConflict)
    ));
    // A re-anchored window equally revokes observers of the older anchor.
    store.clear_compute_control_unheld(&raced).await.unwrap();
    store.mark_compute_control_unheld(&raced, t0).await.unwrap();
    store.clear_compute_control_unheld(&raced).await.unwrap();
    store
        .mark_compute_control_unheld(&raced, t0 - 1)
        .await
        .unwrap();
    assert!(matches!(
        store
            .fail_expired_compute_control(
                &raced,
                "ERR_DEADLINE_EXCEEDED",
                "settler racing a re-anchor",
                t0
            )
            .await,
        Err(UserAppStoreError::VersionConflict)
    ));
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn turso_compute_control_deadline_binding_and_expiry_settlement() {
    let directory = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&directory.path().join("compute-deadline.db"))
        .await
        .unwrap();
    contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicitly disposable PostgreSQL DSN; draft has not been executed"]
async fn pg_compute_control_deadline_binding_and_expiry_settlement() {
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

/// Codex-reproduced concurrency regression (fixed): with the app-row lock
/// removed from the anchor bookkeeping, a concurrent clear could commit
/// BETWEEN the settler's anchor verification and its terminal CAS, yielding
/// `failed, anchor=NULL` — a terminalized control whose lease had come back
/// to life after the settler's observation. The settler now holds the anchor
/// row lock from its locked re-verification until commit, so the clear
/// either commits first (settler refuses) or blocks until after the terminal
/// CAS (harmless post-terminal cleanup). This test proves the gap is closed
/// with explicit permits — no timing-based assumption.
#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicitly disposable PostgreSQL DSN; run alongside the PG contract"]
async fn pg_expire_anchor_lock_blocks_a_concurrent_clear_inside_the_gap() {
    use super::transaction_fault_tests::{ExpireAnchorProbe, with_expire_anchor_probe};
    use std::{sync::Arc, time::Duration};

    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG DSN required")),
        max_connections: Some(1),
        min_connections: Some(1),
        statement_timeout_secs: Some(30),
        ..Default::default()
    };
    // Independent owners: the paused settler never blocks the clearer.
    let settler = Arc::new(ToastyUserAppStore::connect(&config).await.unwrap());
    let clearer = ToastyUserAppStore::connect(&config).await.unwrap();
    let run = uuid::Uuid::new_v4().simple().to_string();
    let now = chrono::Utc::now().timestamp_millis();

    let mut record = claimed_control(
        &settler,
        &format!("expirelock{run}"),
        ComputeControlAction::Restart,
    )
    .await;
    let identity = ComputeExecutorIdentity {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        scope: record.scope,
        operation_id: record.operation_id.clone(),
        generation: record.generation,
        executor_id: record.executor_id.clone().unwrap(),
    };
    record = settler
        .bind_compute_lease(
            &identity,
            &UserAppOperationLeaseReceipt::Kubernetes {
                service_type: ServiceType::UserappBuilder,
                namespace: "rcoder-builder".into(),
                name: format!("rcoder-app-builder-expirelock{run}"),
                uid: "f".repeat(36),
                resource_version: "9".into(),
                token: record.executor_id.clone().unwrap(),
            },
        )
        .await
        .unwrap();
    record = settler
        .advance_compute_control(&ComputeControlProgress {
            identity: identity.clone(),
            expected_revision: record.revision,
            state: ComputeControlState::RecoveryRequired,
            stage: ComputeControlStage::Stopping,
            checkpoint: serde_json::json!({"witness": true}),
            error_code: Some("ERR_STOP_REPLY_TIMEOUT".into()),
            error_message: Some("fixture".into()),
        })
        .await
        .unwrap();
    settler
        .bind_compute_control_deadline(&record, now - 1)
        .await
        .unwrap();
    let anchor = now - 400_000;
    settler
        .mark_compute_control_unheld(&record, anchor)
        .await
        .unwrap();

    // The settler holds the anchor row lock and pauses between its locked
    // re-verification and the terminal CAS.
    let probe = Arc::new(ExpireAnchorProbe::default());
    let settling = {
        let settler = Arc::clone(&settler);
        let snapshot = record.clone();
        let probe = Arc::clone(&probe);
        tokio::spawn(async move {
            settler
                .run(false, move |tx, backend| {
                    let (snapshot, probe) = (snapshot.clone(), Arc::clone(&probe));
                    Box::pin(with_expire_anchor_probe(probe, async move {
                        super::compute_recovery::expire(
                            tx,
                            backend,
                            &snapshot,
                            "ERR_DEADLINE_EXCEEDED",
                            "concurrency gap fixture",
                            anchor,
                        )
                        .await
                    }))
                })
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), probe.reached())
        .await
        .expect("settler must reach the deterministic post-lock boundary");

    // Inside the gap, the concurrent clear must NOT be able to commit: the
    // settler's row lock blocks it until the terminal CAS commits. The
    // blocking itself is deterministic — the lock holder is explicitly
    // paused — so a short timeout proves the lock, not a race heuristic.
    // Pre-fix, this clear committed immediately and the settler still
    // terminalized, yielding failed+anchor=NULL.
    let blocked = tokio::time::timeout(
        Duration::from_millis(300),
        clearer.clear_compute_control_unheld(&record),
    )
    .await;
    assert!(
        blocked.is_err(),
        "the clear must block on the anchor row lock"
    );

    // Release the settler: the terminal CAS commits under the held lock.
    probe.release();
    let settled = tokio::time::timeout(Duration::from_secs(10), settling)
        .await
        .expect("settler task must finish")
        .expect("settler task must not panic")
        .expect("the settler terminalizes while holding the anchor lock");
    assert_eq!(settled.state, ComputeControlState::Failed);

    // The blocked clear retries after commit: clearing a terminal record's
    // anchor is a harmless post-terminal cleanup.
    clearer
        .clear_compute_control_unheld(&settled)
        .await
        .unwrap();
    let after = clearer
        .get_compute_control(&settled.app_id, &settled.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.state, ComputeControlState::Failed);
    settler.shutdown().await.unwrap();
    clearer.shutdown().await.unwrap();
}
