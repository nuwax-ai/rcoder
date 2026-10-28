//! The real runtime-updated predecessor, before its next write's durable intent.
use super::ToastyUserAppStore;
use shared_types::*;

async fn predecessor(
    store: &ToastyUserAppStore,
    pg: bool,
    recovering: bool,
    expired: bool,
    restart: bool,
) -> (UserAppOperationRecord, ComputeExecutorIdentity) {
    let app = store
        .ensure_identity(&format!("updated{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let input = UserAppExecutionInput::new(
        serde_json::json!({"version":1,"restart":restart,"params":{},"previous":{},
            "request":{"pg":pg.then(|| serde_json::json!({"username":"test_role","password":"fixture-only"}))}})
        .to_string(),
    );
    let UserAppAdmissionOutcome::Accepted(pending) = store
        .admit_with_input(
            &UserAppAdmission {
                app_id: app.app_id.clone(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: Some(uuid::Uuid::new_v4().to_string()),
                request_fingerprint: "b".repeat(64),
                kind: if restart {
                    UserAppOperationKind::RestartDeployment
                } else {
                    UserAppOperationKind::StartDeployment
                },
                command: Some(UserAppControlCommand::Deploy {
                    restart,
                    serving_recovery: false,
                    input_digest: input.digest(),
                }),
                metadata: None,
                runtime_policy_on_success: None,
            },
            Some(&input),
        )
        .await
        .unwrap()
    else {
        panic!("fresh predecessor required");
    };
    let context = UserAppExecutionContext {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        operation_id: pending.operation_id.clone(),
        executor_id: "original-deployment-worker".into(),
        request_fingerprint: pending.request_fingerprint.clone(),
    };
    let target = UserAppMutationTarget {
        context,
        resource: AppResourceIdentity {
            kind: AppResourceKind::Deployment,
            name: format!("rcoder-app-{}", app.app_id),
            uid: "original-workload-uid".into(),
            resource_version: Some("7".into()),
        },
    };
    let checkpoint = serde_json::json!({"target":target,
        "resource":{"container_id":target.resource.name,"project_id":app.app_id},
        "storage_target":null,"requested_storage_size":null});
    let mut old = store
        .advance(&progress(&pending, "runtime_updated", checkpoint))
        .await
        .unwrap();
    store
        .bind_operation_deadline(
            &old.app_id,
            &old.operation_id,
            &old.lifecycle_id,
            chrono::Utc::now().timestamp_millis() + if expired { -1 } else { 60_000 },
        )
        .await
        .unwrap();
    if recovering {
        let mut recovery = progress(&old, &old.step, old.checkpoint.clone());
        recovery.state = UserAppOperationState::RecoveryRequired;
        old = store.advance(&recovery).await.unwrap();
    }
    let next = store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app.app_id,
            lifecycle_id: app.lifecycle_id,
            scope: UserAppOperationScope::Prod,
            operation_id: uuid::Uuid::new_v4().to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            request_fingerprint: "a".repeat(64),
            action: ComputeControlAction::Restart,
            restart_image_roll: false,
            builder_startup_mode: Default::default(),
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: next.app_id.clone(),
        lifecycle_id: next.lifecycle_id.clone(),
        scope: next.scope,
        operation_id: next.operation_id.clone(),
        generation: next.generation,
        executor_id: "successor-worker".into(),
    };
    let next = store
        .claim_compute_control(&identity, next.revision)
        .await
        .unwrap();
    assert!(next.interrupted_operations.contains(&old.operation_id));
    assert!(
        store
            .get_operation_lease(&old.app_id, &old.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    (old, identity)
}

fn progress(
    old: &UserAppOperationRecord,
    step: &str,
    checkpoint: serde_json::Value,
) -> UserAppOperationProgress {
    UserAppOperationProgress {
        app_id: old.app_id.clone(),
        lifecycle_id: old.lifecycle_id.clone(),
        operation_id: old.operation_id.clone(),
        expected_revision: old.revision,
        executor_id: "original-deployment-worker".into(),
        state: UserAppOperationState::Running,
        step: step.into(),
        checkpoint,
        error_code: None,
        error_message: None,
    }
}

async fn update_boundary_contract(store: &ToastyUserAppStore) {
    let (within_budget, _) = predecessor(store, true, false, false, true).await;
    assert!(
        store
            .expire_deployment_update(&within_budget)
            .await
            .is_err(),
        "normal in-budget deployment must remain running"
    );
    assert_eq!(
        store
            .get_operation(&within_budget.app_id, &within_budget.operation_id)
            .await
            .unwrap(),
        Some(within_budget)
    );
    let (missing_deadline, _) = predecessor(store, true, false, true, true).await;
    let id = missing_deadline.operation_id.clone();
    store
        .run(false, move |tx, backend| {
            let id = id.clone();
            Box::pin(async move {
                toasty::sql::statement(super::repo::sql(
                    backend,
                    "DELETE FROM userapp_operation_deadlines WHERE operation_id=$1",
                ))
                .bind(id)
                .exec(tx)
                .await
                .map_err(super::storage)?;
                Ok(())
            })
        })
        .await
        .unwrap();
    assert!(
        store
            .expire_deployment_update(&missing_deadline)
            .await
            .is_err(),
        "a missing budget is not permission to retire a request"
    );
    let (start, _) = predecessor(store, true, false, true, false).await;
    assert_eq!(
        store.expire_deployment_update(&start).await.unwrap().state,
        UserAppOperationState::Failed,
        "start and restart share the same protected deployment boundary"
    );
    for recovering in [false, true] {
        let (old, _identity) = predecessor(store, true, recovering, true, true).await;
        let terminal = store
            .expire_deployment_update(&old)
            .await
            .expect("returned runtime update before the credential intent is drainable");
        assert_eq!(terminal.state, UserAppOperationState::Failed);
        assert_eq!(terminal.step, "deployment_update_expired");
        assert_eq!(
            terminal.checkpoint["deployment_update_expiry"]["deployment_completed"],
            false
        );
        assert_eq!(terminal.checkpoint["target"], old.checkpoint["target"]);
        assert_eq!(terminal.checkpoint["resource"], old.checkpoint["resource"]);
        assert!(
            store
                .advance(&progress(
                    &old,
                    "explicit_pg_applying",
                    old.checkpoint.clone()
                ))
                .await
                .is_err(),
            "the displaced worker cannot cross the next write intent"
        );
        assert!(
            store
                .get_application(&old.app_id)
                .await
                .unwrap()
                .unwrap()
                .active_operations
                .prod
                .is_none()
        );
    }
    let (old, _identity) = predecessor(store, false, false, true, true).await;
    assert!(
        store.expire_deployment_update(&old).await.is_err(),
        "without explicit pg, a later SQL write need not cross this checkpoint"
    );
    let (old, _identity) = predecessor(store, true, false, true, true).await;
    let submitted = store
        .advance(&progress(
            &old,
            "explicit_pg_applying",
            old.checkpoint.clone(),
        ))
        .await
        .unwrap();
    assert!(
        store.expire_deployment_update(&old).await.is_err(),
        "a stale observer cannot retire a newly submitted phase"
    );
    assert!(
        store.expire_deployment_update(&submitted).await.is_err(),
        "an unknown database write still needs its own result receipt"
    );
    assert_eq!(
        store
            .get_operation(&old.app_id, &old.operation_id)
            .await
            .unwrap(),
        Some(submitted)
    );

    // A returned update must belong to this exact operation and workload.
    for field in [
        "executor_id",
        "lifecycle_id",
        "operation_id",
        "request_fingerprint",
    ] {
        let (old, _identity) = predecessor(store, true, false, true, true).await;
        let mut changed = old.checkpoint.clone();
        changed["target"]["context"][field] = "different-identity".into();
        let changed = store
            .advance(&progress(&old, &old.step, changed))
            .await
            .unwrap();
        assert!(
            store.expire_deployment_update(&changed).await.is_err(),
            "{field}"
        );
    }
    // A newer/other input protocol must establish its own next-write barrier.
    let (old, _) = predecessor(store, true, false, true, true).await;
    for value in [
        serde_json::json!({"version":2,"restart":true,"params":{},"previous":{},"request":{"pg":{"username":"role","password":"fixture-only"}}}),
        serde_json::json!({"version":1,"restart":true,"params":{},"previous":{},"request":{"pg":null}}),
        serde_json::json!({"version":1,"restart":true,"params":null,"previous":{},"request":{"pg":{"username":"role","password":"fixture-only"}}}),
    ] {
        let input = UserAppExecutionInput::new(value.to_string());
        let mut projected = old.clone();
        projected.command = Some(UserAppControlCommand::Deploy {
            restart: true,
            serving_recovery: false,
            input_digest: input.digest(),
        });
        assert!(!userapp_deployment_update_precedes_credentials(
            &projected, &input
        ));
    }
}

async fn update_boundary_race(first: &ToastyUserAppStore, second: &ToastyUserAppStore) {
    let (old, _identity) = predecessor(first, true, false, true, true).await;
    let next = progress(&old, "explicit_pg_applying", old.checkpoint.clone());
    let (drain, writer) =
        tokio::join!(first.expire_deployment_update(&old), second.advance(&next),);
    match (drain, writer) {
        (Ok(terminal), Err(UserAppStoreError::VersionConflict)) => {
            assert_eq!(terminal.state, UserAppOperationState::Failed);
        }
        (Err(UserAppStoreError::VersionConflict), Ok(submitted)) => {
            assert_eq!(submitted.step, "explicit_pg_applying");
            assert_eq!(submitted.state, UserAppOperationState::Running);
        }
        (drain, writer) => {
            panic!("exactly one boundary transition must win: drain={drain:?}, writer={writer:?}")
        }
    }
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn interrupted_deployment_update_contract() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("updates.db"))
        .await
        .unwrap();
    update_boundary_contract(&store).await;
    update_boundary_race(&store, &store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires an explicitly disposable PostgreSQL database"]
async fn pg_interrupted_deployment_update_contract() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        max_connections: Some(1),
        min_connections: Some(1),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    let second = ToastyUserAppStore::connect(&config).await.unwrap();
    update_boundary_contract(&store).await;
    update_boundary_race(&store, &second).await;
    second.shutdown().await.unwrap();
    store.shutdown().await.unwrap();
}
