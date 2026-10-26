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
