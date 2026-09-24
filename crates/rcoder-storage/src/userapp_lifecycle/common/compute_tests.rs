use super::ToastyUserAppStore;
use shared_types::*;
fn request(
    app: &UserAppLifecycleRecord,
    id: &str,
    action: ComputeControlAction,
) -> ComputeControlRequest {
    ComputeControlRequest {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: UserAppOperationScope::Dev,
        operation_id: id.into(),
        request_id: id.into(),
        request_fingerprint: "a".repeat(64),
        action,
        restart_image_roll: false,
    }
}

#[tokio::test]
async fn automatic_repair_only_claims_idle_scope_and_explicit_restart_supersedes_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("auto-repair.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("autorepairapp").await.unwrap();
    let ordinary = store
        .admit(&UserAppAdmission {
            app_id: app.app_id.clone(),
            lifecycle_id: Some(app.lifecycle_id.clone()),
            operation_id: "buildone".into(),
            request_id: Some("buildrequest".into()),
            request_fingerprint: "b".repeat(64),
            kind: UserAppOperationKind::EnsureBuilder,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .unwrap();
    assert!(matches!(ordinary, UserAppAdmissionOutcome::Accepted(_)));
    let mut auto = request(&app, "autorepairone", ComputeControlAction::Restart);
    auto.request_id = "auto-repair-physicalone".into();
    assert!(matches!(
        store.admit_idle_compute_repair(&auto).await,
        Err(UserAppStoreError::VersionConflict)
    ));
    assert!(
        store
            .get_compute_control(&app.app_id, &auto.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    store.shutdown().await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("idle-repair.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("idleautorepairapp").await.unwrap();
    let mut auto = request(&app, "autorepairtwo", ComputeControlAction::Restart);
    auto.request_id = "auto-repair-physicaltwo".into();
    let admitted = store.admit_idle_compute_repair(&auto).await.unwrap();
    let executor = ComputeExecutorIdentity {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: UserAppOperationScope::Dev,
        operation_id: admitted.operation_id.clone(),
        generation: admitted.generation,
        executor_id: "repairworker".into(),
    };
    store
        .claim_compute_control(&executor, admitted.revision)
        .await
        .unwrap();
    let explicit = store
        .admit_compute_control(&request(
            &app,
            "manualrestart",
            ComputeControlAction::Restart,
        ))
        .await
        .unwrap();
    assert!(
        explicit
            .interrupted_operations
            .contains(&admitted.operation_id)
    );
    assert!(matches!(
        store.check_compute_executor(&executor).await,
        Err(UserAppStoreError::VersionConflict)
    ));
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn compute_priority_rejects_new_busy_requests_without_releasing_business_slot() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("compute.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("computeapp").await.unwrap();
    let ordinary = store
        .admit(&UserAppAdmission {
            app_id: app.app_id.clone(),
            lifecycle_id: Some(app.lifecycle_id.clone()),
            operation_id: "oldbusiness".into(),
            request_id: Some("businessrequest".into()),
            request_fingerprint: "b".repeat(64),
            kind: UserAppOperationKind::EnsureBuilder,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .unwrap();
    assert!(matches!(ordinary, UserAppAdmissionOutcome::Accepted(_)));
    let restart = store
        .admit_compute_control(&request(&app, "restartone", ComputeControlAction::Restart))
        .await
        .unwrap();
    assert_eq!(restart.interrupted_operations, vec!["oldbusiness"]);
    let repeated = store
        .admit_compute_control(&request(&app, "restarttwo", ComputeControlAction::Restart))
        .await
        .unwrap_err();
    assert!(matches!(
        repeated,
        UserAppStoreError::OperationInProgress(_)
    ));
    let stop = store
        .admit_compute_control(&request(&app, "stopone", ComputeControlAction::Stop))
        .await
        .unwrap();
    assert_eq!(stop.generation, restart.generation + 1);
    assert!(stop.interrupted_operations.contains(&restart.operation_id));
    assert_eq!(
        store
            .get_compute_control(&app.app_id, &restart.operation_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        ComputeControlState::Superseded
    );
    let blocked_restart = store
        .admit_compute_control(&request(
            &app,
            "restartthree",
            ComputeControlAction::Restart,
        ))
        .await
        .unwrap_err();
    let UserAppStoreError::OperationInProgress(blocker) = blocked_restart else {
        panic!("expected conflict")
    };
    assert_eq!(blocker.operation_id, stop.operation_id);
    assert!(
        store
            .get_compute_control(&app.app_id, "restartthree")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .get_application(&app.app_id)
            .await
            .unwrap()
            .unwrap()
            .active_operations
            .dev
            .as_deref(),
        Some("oldbusiness")
    );
    // Only the operation-originating request is durably replayable.
    let replay = store
        .admit_compute_control(&request(&app, "restartone", ComputeControlAction::Restart))
        .await
        .unwrap();
    assert_eq!(replay.operation_id, restart.operation_id);
    assert_eq!(replay.state, ComputeControlState::Superseded);
    let mut changed = request(&app, "restartone", ComputeControlAction::Restart);
    changed.request_fingerprint = "c".repeat(64);
    assert!(store.admit_compute_control(&changed).await.is_err());
    let mut prod = request(&app, "prodrestart", ComputeControlAction::Restart);
    prod.scope = UserAppOperationScope::Prod;
    let prod = store.admit_compute_control(&prod).await.unwrap();
    assert_eq!(prod.generation, 1);
    assert!(prod.interrupted_operations.is_empty());
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn compute_claim_retires_only_never_claimed_business_without_faking_execution() {
    for previously_running in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = ToastyUserAppStore::open_exclusive(&dir.path().join("pending.db"))
            .await
            .unwrap();
        let app = store.ensure_identity("pendingapp").await.unwrap();
        let UserAppAdmissionOutcome::Accepted(mut old) = store
            .admit(&UserAppAdmission {
                app_id: app.app_id.clone(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                operation_id: "oldpending".into(),
                request_id: Some("originalrequest".into()),
                request_fingerprint: "a".repeat(64),
                kind: UserAppOperationKind::EnsureBuilder,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .unwrap()
        else {
            panic!("accepted")
        };
        let progress = UserAppOperationProgress {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            operation_id: old.operation_id.clone(),
            expected_revision: old.revision,
            executor_id: "businessworker".into(),
            state: UserAppOperationState::Running,
            step: "claimed".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        };
        if previously_running {
            old = store.advance(&progress).await.unwrap();
        }
        let stop = store
            .admit_compute_control(&request(&app, "stop", ComputeControlAction::Stop))
            .await
            .unwrap();
        let identity = ComputeExecutorIdentity {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: stop.operation_id.clone(),
            generation: stop.generation,
            executor_id: "controlworker".into(),
        };
        store
            .claim_compute_control(&identity, stop.revision)
            .await
            .unwrap();
        let observed = store
            .get_operation(&app.app_id, &old.operation_id)
            .await
            .unwrap()
            .unwrap();
        let current = store.get_application(&app.app_id).await.unwrap().unwrap();
        if previously_running {
            assert_eq!(observed, old, "claim must not invent a runtime drain proof");
            assert_eq!(
                current.active_operations.slot(UserAppOperationScope::Dev),
                Some(&old.operation_id)
            );
        } else {
            assert_eq!(observed.state, UserAppOperationState::Failed);
            assert_eq!(observed.step, "cancelled_before_execution");
            assert_eq!(observed.executor_id, None, "do not invent an executor");
            assert_eq!(observed.request_id, old.request_id, "keep original history");
            assert_eq!(
                observed.checkpoint["compute_operation_id"],
                stop.operation_id
            );
            assert!(
                current
                    .active_operations
                    .slot(UserAppOperationScope::Dev)
                    .is_none()
            );
            assert!(
                store.advance(&progress).await.is_err(),
                "old worker cannot claim after cancellation"
            );
        }
        store.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn compute_executor_is_single_winner_and_stop_fences_previous_generation() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("claim.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("claimapp").await.unwrap();
    let restart = store
        .admit_compute_control(&request(&app, "restart", ComputeControlAction::Restart))
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: UserAppOperationScope::Dev,
        operation_id: restart.operation_id.clone(),
        generation: restart.generation,
        executor_id: "firstworker".into(),
    };
    let mut contender = identity.clone();
    contender.executor_id = "secondworker".into();
    let (first, second) = tokio::join!(
        store.claim_compute_control(&identity, 1),
        store.claim_compute_control(&contender, 1)
    );
    assert_ne!(first.is_ok(), second.is_ok());
    let winner = if first.is_ok() { &identity } else { &contender };
    store.check_compute_executor(winner).await.unwrap();
    store
        .admit_compute_control(&request(&app, "stop", ComputeControlAction::Stop))
        .await
        .unwrap();
    assert!(matches!(
        store.check_compute_executor(winner).await,
        Err(UserAppStoreError::VersionConflict)
    ));
    assert!(store.claim_compute_control(winner, 2).await.is_err());
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn compute_stop_rejects_late_business_success_and_preserves_recovery_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("late.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("lateapp").await.unwrap();
    let outcome = store
        .admit(&UserAppAdmission {
            app_id: app.app_id.clone(),
            lifecycle_id: Some(app.lifecycle_id.clone()),
            operation_id: "oldop".into(),
            request_id: Some("oldreq".into()),
            request_fingerprint: "a".repeat(64),
            kind: UserAppOperationKind::EnsureBuilder,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .unwrap();
    let UserAppAdmissionOutcome::Accepted(op) = outcome else {
        panic!("expected new operation")
    };
    let mut progress = UserAppOperationProgress {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        operation_id: op.operation_id,
        expected_revision: op.revision,
        executor_id: "worker".into(),
        state: UserAppOperationState::Running,
        step: "creating".into(),
        checkpoint: serde_json::json!({"write":"submitted"}),
        error_code: None,
        error_message: None,
    };
    let running = store.advance(&progress).await.unwrap();
    progress.expected_revision = running.revision;
    let context = UserAppExecutionContext {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        operation_id: running.operation_id.clone(),
        executor_id: "worker".into(),
        request_fingerprint: "a".repeat(64),
    };
    store.check_business_execution(&context).await.unwrap();
    store
        .admit_compute_control(&request(&app, "stop", ComputeControlAction::Stop))
        .await
        .unwrap();
    assert!(matches!(
        store.check_business_execution(&context).await,
        Err(UserAppStoreError::VersionConflict)
    ));
    progress.state = UserAppOperationState::Succeeded;
    assert!(matches!(
        store.advance(&progress).await,
        Err(UserAppStoreError::VersionConflict)
    ));
    progress.state = UserAppOperationState::Running;
    let checkpoint = store.advance(&progress).await.unwrap();
    progress.expected_revision = checkpoint.revision;
    progress.state = UserAppOperationState::RecoveryRequired;
    let uncertain = store.advance(&progress).await.unwrap();
    assert_eq!(
        uncertain.checkpoint,
        serde_json::json!({"write":"submitted"})
    );
    assert_eq!(
        store
            .get_application(&app.app_id)
            .await
            .unwrap()
            .unwrap()
            .active_operations
            .dev
            .as_deref(),
        Some("oldop")
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn compute_intent_blocks_new_same_scope_work_but_not_other_scope() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("intent.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("intentapp").await.unwrap();
    let stop = store
        .admit_compute_control(&request(&app, "stop", ComputeControlAction::Stop))
        .await
        .unwrap();
    assert!(
        store
            .compute_desired_stopped(&app.app_id, &app.lifecycle_id, UserAppOperationScope::Dev)
            .await
            .unwrap()
    );
    let mut normal = UserAppAdmission {
        app_id: app.app_id.clone(),
        lifecycle_id: Some(app.lifecycle_id.clone()),
        operation_id: "ensure".into(),
        request_id: Some("ensure".into()),
        request_fingerprint: "a".repeat(64),
        kind: UserAppOperationKind::EnsureBuilder,
        command: None,
        metadata: None,
        runtime_policy_on_success: None,
    };
    let error = store.admit(&normal).await.unwrap_err();
    let UserAppStoreError::OperationInProgress(blocker) = error else {
        panic!("expected structured blocker")
    };
    assert_eq!(blocker.operation_id, stop.operation_id);
    normal.kind = UserAppOperationKind::Start;
    assert!(store.admit(&normal).await.is_ok());
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn compute_restart_rejected_during_stop_can_be_resubmitted_after_completion() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("sequence.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("sequenceapp").await.unwrap();
    let stop = store
        .admit_compute_control(&request(&app, "stop", ComputeControlAction::Stop))
        .await
        .unwrap();
    let restart_request = request(&app, "restart", ComputeControlAction::Restart);
    assert!(matches!(
        store.admit_compute_control(&restart_request).await,
        Err(UserAppStoreError::OperationInProgress(_))
    ));
    assert!(
        store
            .get_compute_control(&app.app_id, "restart")
            .await
            .unwrap()
            .is_none()
    );
    // Storage contract only; runtime exit proof is supplied by the coordinator.
    let identity = compute_identity(&stop);
    store
        .claim_compute_control(&identity, stop.revision)
        .await
        .unwrap();
    let mut stopped = store
        .bind_compute_lease(&identity, &compute_receipt())
        .await
        .unwrap();
    for stage in [ComputeControlStage::Stopping, ComputeControlStage::Stopped] {
        stopped = store
            .advance_compute_control(&compute_progress(&stopped, stage))
            .await
            .unwrap();
    }
    let mut finish = compute_progress(&stopped, ComputeControlStage::Completed);
    finish.state = ComputeControlState::Succeeded;
    store.advance_compute_control(&finish).await.unwrap();
    store
        .forget_compute_lease(&identity, &compute_receipt())
        .await
        .unwrap();
    // Failed admission creates no queue. An explicit retry now creates the operation.
    let restart = store.admit_compute_control(&restart_request).await.unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: UserAppOperationScope::Dev,
        operation_id: restart.operation_id.clone(),
        generation: restart.generation,
        executor_id: "worker".into(),
    };
    store.claim_compute_control(&identity, 1).await.unwrap();
    store.check_compute_executor(&identity).await.unwrap();
    let newer_stop = store
        .admit_compute_control(&request(&app, "newstop", ComputeControlAction::Stop))
        .await
        .unwrap();
    assert!(newer_stop.generation > restart.generation);
    assert!(store.check_compute_executor(&identity).await.is_err());
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL database"]
async fn compute_pg_independent_connections_preserve_stop_priority() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        max_connections: Some(1),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let first = ToastyUserAppStore::connect(&config).await.unwrap();
    let second = ToastyUserAppStore::connect(&config).await.unwrap();
    for _ in 0..12 {
        let app = first
            .ensure_identity(&format!("priority{}", uuid::Uuid::new_v4().simple()))
            .await
            .unwrap();
        let stop_request = request(
            &app,
            &format!("stop{}", uuid::Uuid::new_v4().simple()),
            ComputeControlAction::Stop,
        );
        let old_id = format!("pending{}", uuid::Uuid::new_v4().simple());
        first
            .admit(&UserAppAdmission {
                app_id: app.app_id.clone(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                operation_id: old_id.clone(),
                request_id: None,
                request_fingerprint: "c".repeat(64),
                kind: UserAppOperationKind::EnsureBuilder,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .unwrap();
        let restart_request = request(
            &app,
            &format!("restart{}", uuid::Uuid::new_v4().simple()),
            ComputeControlAction::Restart,
        );
        let (stop, restart) = tokio::join!(
            first.admit_compute_control(&stop_request),
            second.admit_compute_control(&restart_request)
        );
        let stop = stop.expect("Stop wins either ordering");
        match restart {
            Ok(restart) => {
                let old = second
                    .get_compute_control(&app.app_id, &restart.operation_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(old.state, ComputeControlState::Superseded);
                assert!(stop.generation > restart.generation);
            }
            Err(UserAppStoreError::OperationInProgress(blocker)) => {
                assert_eq!(blocker.operation_id, stop.operation_id)
            }
            Err(error) => panic!("unexpected race error: {error}"),
        }
        let identity = ComputeExecutorIdentity {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: stop.operation_id.clone(),
            generation: stop.generation,
            executor_id: "workerone".into(),
        };
        let mut other = identity.clone();
        other.executor_id = "workertwo".into();
        let (a, b) = tokio::join!(
            first.claim_compute_control(&identity, stop.revision),
            second.claim_compute_control(&other, stop.revision)
        );
        assert_ne!(a.is_ok(), b.is_ok());
        let retired = second
            .get_operation(&app.app_id, &old_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retired.state, UserAppOperationState::Failed);
        assert_eq!(retired.executor_id, None);
        assert_eq!(
            retired.checkpoint["compute_operation_id"],
            stop.operation_id
        );
        let winner = if a.is_ok() {
            identity.clone()
        } else {
            other.clone()
        };
        let loser = match (a, b) {
            (Ok(_), Err(error)) | (Err(error), Ok(_)) => error,
            _ => panic!("exactly one executor must win"),
        };
        assert!(matches!(loser, UserAppStoreError::VersionConflict));
        let receipt = compute_receipt();
        let mut record = first.bind_compute_lease(&winner, &receipt).await.unwrap();
        for stage in [
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Completed,
        ] {
            let mut progress = compute_progress(&record, stage);
            progress.identity = winner.clone();
            if stage == ComputeControlStage::Completed {
                progress.state = ComputeControlState::Succeeded;
            }
            record = second.advance_compute_control(&progress).await.unwrap();
            assert!(first.advance_compute_control(&progress).await.is_err());
        }
        first.forget_compute_lease(&winner, &receipt).await.unwrap();
        assert!(
            second
                .get_compute_control(&app.app_id, &stop.operation_id)
                .await
                .unwrap()
                .unwrap()
                .lease
                .is_none()
        );
    }
    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
}

fn compute_identity(record: &ComputeControlRecord) -> ComputeExecutorIdentity {
    ComputeExecutorIdentity {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        scope: record.scope,
        operation_id: record.operation_id.clone(),
        generation: record.generation,
        executor_id: "controlworker".into(),
    }
}
fn compute_receipt() -> UserAppOperationLeaseReceipt {
    UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "test".into(),
        name: "builderlease".into(),
        uid: "leaseuid".into(),
        resource_version: "123".into(),
        token: "leasetoken".into(),
    }
}
fn compute_progress(
    record: &ComputeControlRecord,
    stage: ComputeControlStage,
) -> ComputeControlProgress {
    ComputeControlProgress {
        identity: compute_identity(record),
        expected_revision: record.revision,
        state: ComputeControlState::Running,
        stage,
        checkpoint: serde_json::json!({"fixture":"storage contract only"}),
        error_code: None,
        error_message: None,
    }
}

#[tokio::test]
async fn compute_completion_requires_ordered_progress_and_exact_receipt_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("completion.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("completionapp").await.unwrap();
    let pending = store
        .admit_compute_control(&request(&app, "stop", ComputeControlAction::Stop))
        .await
        .unwrap();
    let identity = compute_identity(&pending);
    let mut record = store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let mut premature = compute_progress(&record, ComputeControlStage::Completed);
    premature.state = ComputeControlState::Succeeded;
    assert!(store.advance_compute_control(&premature).await.is_err());
    let receipt = compute_receipt();
    record = store.bind_compute_lease(&identity, &receipt).await.unwrap();
    assert_eq!(
        store.bind_compute_lease(&identity, &receipt).await.unwrap(),
        record
    );
    let mut wrong = receipt.clone();
    if let UserAppOperationLeaseReceipt::Kubernetes { uid, .. } = &mut wrong {
        *uid = "foreignuid".into();
    }
    assert!(store.bind_compute_lease(&identity, &wrong).await.is_err());
    assert!(
        store
            .forget_compute_lease(&identity, &receipt)
            .await
            .is_err()
    );
    for stage in [ComputeControlStage::Stopping, ComputeControlStage::Stopped] {
        let progress = compute_progress(&record, stage);
        record = store.advance_compute_control(&progress).await.unwrap();
        assert!(
            store.advance_compute_control(&progress).await.is_err(),
            "stale CAS must fail"
        );
    }
    let mut finish = compute_progress(&record, ComputeControlStage::Completed);
    finish.state = ComputeControlState::Succeeded;
    let completed = store.advance_compute_control(&finish).await.unwrap();
    assert_eq!(completed.lease, Some(receipt.clone()));
    assert!(
        !serde_json::to_string(&completed)
            .unwrap()
            .contains("leasetoken")
    );
    // A new request can proceed after completion; old receipt cleanup must not alter it.
    let restart = store
        .admit_compute_control(&request(&app, "restart", ComputeControlAction::Restart))
        .await
        .unwrap();
    assert!(store.forget_compute_lease(&identity, &wrong).await.is_err());
    store
        .forget_compute_lease(&identity, &receipt)
        .await
        .unwrap();
    store
        .forget_compute_lease(&identity, &receipt)
        .await
        .unwrap();
    assert_eq!(
        store
            .get_compute_control(&app.app_id, &restart.operation_id)
            .await
            .unwrap()
            .unwrap(),
        restart
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn compute_superseded_executor_cannot_commit_or_forget_runtime_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("superseded.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("supersededapp").await.unwrap();
    let pending = store
        .admit_compute_control(&request(&app, "restart", ComputeControlAction::Restart))
        .await
        .unwrap();
    let identity = compute_identity(&pending);
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let record = store
        .bind_compute_lease(&identity, &compute_receipt())
        .await
        .unwrap();
    let stop = store
        .admit_compute_control(&request(&app, "stop", ComputeControlAction::Stop))
        .await
        .unwrap();
    assert!(
        store
            .advance_compute_control(&compute_progress(&record, ComputeControlStage::Stopping))
            .await
            .is_err()
    );
    assert!(
        store
            .forget_compute_lease(&identity, &compute_receipt())
            .await
            .is_err()
    );
    let stop_identity = compute_identity(&stop);
    store
        .claim_compute_control(&stop_identity, stop.revision)
        .await
        .unwrap();
    let stop = store
        .bind_compute_lease(&stop_identity, &compute_receipt())
        .await
        .unwrap();
    assert!(
        store
            .advance_compute_control(&compute_progress(&stop, ComputeControlStage::Stopping))
            .await
            .is_err(),
        "superseded is not drained"
    );
    assert_eq!(
        store
            .get_compute_control(&app.app_id, &record.operation_id)
            .await
            .unwrap()
            .unwrap()
            .lease,
        Some(compute_receipt())
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn compute_uncertain_write_cannot_be_downgraded_to_failure_or_reclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("uncertain.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("uncertainapp").await.unwrap();
    let pending = store
        .admit_compute_control(&request(&app, "stop", ComputeControlAction::Stop))
        .await
        .unwrap();
    let identity = compute_identity(&pending);
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let record = store
        .bind_compute_lease(&identity, &compute_receipt())
        .await
        .unwrap();
    let record = store
        .advance_compute_control(&compute_progress(&record, ComputeControlStage::Stopping))
        .await
        .unwrap();
    let mut failure = compute_progress(&record, ComputeControlStage::Stopping);
    failure.state = ComputeControlState::Failed;
    failure.error_code = Some("ERR_BACKEND_ERROR".into());
    failure.error_message = Some("Remote write response lost".into());
    assert!(store.advance_compute_control(&failure).await.is_err());
    failure.state = ComputeControlState::RecoveryRequired;
    let uncertain = store.advance_compute_control(&failure).await.unwrap();
    assert_eq!(uncertain.checkpoint, failure.checkpoint);
    assert_eq!(uncertain.error_message, failure.error_message);
    assert!(
        store
            .claim_compute_control(&identity, uncertain.revision)
            .await
            .is_err()
    );
    assert!(
        store
            .forget_compute_lease(&identity, &compute_receipt())
            .await
            .is_err()
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn compute_drain_recovery_accepts_only_initial_policy_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("drain-recovery.db"))
        .await
        .unwrap();
    for (app_id, extra_evidence) in [("cleanrecovery", false), ("uncertainrecovery", true)] {
        let app = store.ensure_identity(app_id).await.unwrap();
        let mut input = request(
            &app,
            &format!("restart{app_id}"),
            ComputeControlAction::Restart,
        );
        input.restart_image_roll = true;
        let pending = store.admit_compute_control(&input).await.unwrap();
        let identity = compute_identity(&pending);
        let claimed = store
            .claim_compute_control(&identity, pending.revision)
            .await
            .unwrap();
        let mut failure = compute_progress(&claimed, ComputeControlStage::DrainingPrevious);
        failure.state = ComputeControlState::RecoveryRequired;
        failure.checkpoint = if extra_evidence {
            serde_json::json!({"restart_image_roll": true, "runtime_write": "unknown"})
        } else {
            claimed.checkpoint.clone()
        };
        failure.error_code = Some("ERR_BACKEND_ERROR".into());
        failure.error_message = Some("Drain interrupted".into());
        let uncertain = store.advance_compute_control(&failure).await.unwrap();
        let resumed = store.resume_compute_drain(&uncertain).await;
        if extra_evidence {
            assert!(resumed.is_err(), "runtime evidence needs inspection");
        } else {
            let resumed = resumed.unwrap();
            assert_eq!(resumed.operation_id, pending.operation_id);
            assert_eq!(resumed.state, ComputeControlState::Pending);
            assert_eq!(resumed.checkpoint, pending.checkpoint);
        }
    }
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn compute_original_executor_drain_ack_releases_only_old_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("drain.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("drainapp").await.unwrap();
    let restart = store
        .admit_compute_control(&request(&app, "restart", ComputeControlAction::Restart))
        .await
        .unwrap();
    let identity = compute_identity(&restart);
    store
        .claim_compute_control(&identity, restart.revision)
        .await
        .unwrap();
    let bound = store
        .bind_compute_lease(&identity, &compute_receipt())
        .await
        .unwrap();
    store
        .advance_compute_control(&compute_progress(&bound, ComputeControlStage::Stopping))
        .await
        .unwrap();
    let stop = store
        .admit_compute_control(&request(&app, "stop", ComputeControlAction::Stop))
        .await
        .unwrap();
    let old = store
        .get_compute_control(&app.app_id, &restart.operation_id)
        .await
        .unwrap()
        .unwrap();
    let mut ack = ComputeControlDrainAcknowledgement {
        identity: identity.clone(),
        expected_revision: old.revision,
        evidence: ComputeControlDrainEvidence::NoMutationSubmitted,
    };
    assert!(
        store.acknowledge_compute_drain(&ack).await.is_err(),
        "submitted write cannot claim no mutation"
    );
    ack.evidence = ComputeControlDrainEvidence::MutationCompleted {
        checkpoint: serde_json::json!({"fixture":"confirmed runtime response"}),
    };
    ack.identity.executor_id = "foreignworker".into();
    assert!(store.acknowledge_compute_drain(&ack).await.is_err());
    ack.identity = identity.clone();
    let drained = store.acknowledge_compute_drain(&ack).await.unwrap();
    assert_eq!(drained.state, ComputeControlState::Failed);
    assert_eq!(drained.lease, Some(compute_receipt()));
    let scan = store.scan_compute_controls(None, 10).await.unwrap();
    assert_eq!(scan.len(), 2);
    store
        .forget_compute_lease(&identity, &compute_receipt())
        .await
        .unwrap();
    let stop_identity = compute_identity(&stop);
    store
        .claim_compute_control(&stop_identity, stop.revision)
        .await
        .unwrap();
    let stop = store
        .bind_compute_lease(&stop_identity, &compute_receipt())
        .await
        .unwrap();
    store
        .advance_compute_control(&compute_progress(&stop, ComputeControlStage::Stopping))
        .await
        .unwrap();
    let scan = store.scan_compute_controls(None, 10).await.unwrap();
    assert_eq!(scan.len(), 1);
    assert_eq!(scan[0].operation_id, stop.operation_id);
    assert!(
        store
            .scan_compute_controls(Some(&stop.operation_id), 10)
            .await
            .unwrap()
            .is_empty()
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn public_repair_prefix_is_rejected_before_control_intent_changes() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("publicrequest.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("publicrequestapp").await.unwrap();
    for action in [ComputeControlAction::Stop, ComputeControlAction::Restart] {
        let mut input = request(&app, "manualrequest", action);
        input.request_id = "auto-repair-manual1".into();
        assert!(matches!(
            store.admit_compute_control(&input).await,
            Err(UserAppStoreError::InvalidOperation(_))
        ));
        assert!(
            store
                .get_compute_control(&app.app_id, &input.operation_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .compute_desired_stopped(&app.app_id, &app.lifecycle_id, UserAppOperationScope::Dev)
                .await
                .unwrap()
        );
    }
    let normal = request(&app, "normalstop", ComputeControlAction::Stop);
    assert_eq!(
        store.admit_compute_control(&normal).await.unwrap().state,
        ComputeControlState::Pending
    );
    store.shutdown().await.unwrap();
}
