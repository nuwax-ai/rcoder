use super::ToastyUserAppStore;
use BusinessStartupState as B;
use CredentialApplicationState as C;
use UserAppOperationScope::{Dev, Prod};
use shared_types::*;

async fn database() -> (
    tempfile::TempDir,
    ToastyUserAppStore,
    UserAppLifecycleRecord,
) {
    let directory = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&directory.path().join("config.turso.db"))
        .await
        .unwrap();
    let app = store.ensure_identity("configapp").await.unwrap();
    (directory, store, app)
}
fn save_request(
    app: &UserAppLifecycleRecord,
    id: &str,
    revision: i64,
    password: &str,
) -> SaveRuntimeConfigurationRequest {
    SaveRuntimeConfigurationRequest {
        lifecycle_id: app.lifecycle_id.clone(),
        request_id: id.into(),
        expected_revision: revision,
        pg: StartPgCredential {
            username: "business".into(),
            password: password.into(),
        },
    }
}
fn admission(
    app: &UserAppLifecycleRecord,
    id: &str,
    command: UserAppControlCommand,
) -> UserAppAdmission {
    UserAppAdmission {
        runtime_policy_on_success: None,
        command: Some(command.clone()),
        metadata: None,
        app_id: app.app_id.clone(),
        lifecycle_id: Some(app.lifecycle_id.clone()),
        operation_id: id.into(),
        request_id: Some(id.into()),
        request_fingerprint: "a".repeat(64),
        kind: command.kind(),
    }
}
fn progress(op: &UserAppOperationRecord, state: UserAppOperationState) -> UserAppOperationProgress {
    UserAppOperationProgress {
        app_id: op.app_id.clone(),
        operation_id: op.operation_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        expected_revision: op.revision,
        executor_id: "configworker".into(),
        state,
        step: "claimed".into(),
        checkpoint: serde_json::Value::Null,
        error_code: None,
        error_message: None,
    }
}
async fn claim(
    store: &ToastyUserAppStore,
    request: &UserAppAdmission,
) -> (UserAppOperationRecord, UserAppExecutionContext) {
    let op = match store.admit(request).await.unwrap() {
        UserAppAdmissionOutcome::Accepted(op) | UserAppAdmissionOutcome::Existing(op) => op,
    };
    claim_admitted(store, op).await
}
async fn claim_admitted(
    store: &ToastyUserAppStore,
    op: UserAppOperationRecord,
) -> (UserAppOperationRecord, UserAppExecutionContext) {
    let op = store
        .advance(&progress(&op, UserAppOperationState::Running))
        .await
        .unwrap();
    let context = UserAppExecutionContext {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        executor_id: "configworker".into(),
        request_fingerprint: op.request_fingerprint.clone(),
    };
    (op, context)
}
// Historical inline deployment capture remains available to exercise persisted
// unknown-write recovery. Ordinary lifecycle admission must never use it.
async fn claim_inline(
    store: &ToastyUserAppStore,
    app: &UserAppLifecycleRecord,
    id: &str,
    password: &str,
) -> (UserAppOperationRecord, UserAppExecutionContext) {
    let pg = StartPgCredential {
        username: "business".into(),
        password: password.into(),
    };
    let input = UserAppExecutionInput::new(serde_json::json!({"request": {"pg": pg}}).to_string());
    let request = admission(
        app,
        id,
        UserAppControlCommand::Deploy {
            restart: false,
            input_digest: input.digest(),
        },
    );
    let op = match store
        .admit_with_configuration(&request, &input, Some(&pg))
        .await
        .unwrap()
    {
        UserAppAdmissionOutcome::Accepted(op) | UserAppAdmissionOutcome::Existing(op) => op,
    };
    claim_admitted(store, op).await
}
fn target() -> RuntimeConfigurationTarget {
    RuntimeConfigurationTarget {
        physical_uid: "physicalone".into(),
        deployment_generation: "generationone".into(),
    }
}
async fn ready(store: &ToastyUserAppStore, context: &UserAppExecutionContext, version: i64) {
    store
        .bind_runtime_configuration_target(context, version, &target())
        .await
        .unwrap();
    store
        .record_runtime_configuration_result(
            context,
            version,
            &target(),
            C::Applying,
            B::NotStarted,
        )
        .await
        .unwrap();
    store
        .record_runtime_configuration_result(context, version, &target(), C::Applied, B::Starting)
        .await
        .unwrap();
    store
        .record_runtime_configuration_result(context, version, &target(), C::Applied, B::Ready)
        .await
        .unwrap();
}

#[tokio::test]
async fn confirmed_preflight_failure_closes_intent_without_promoting_credentials() {
    let (_directory, store, app) = database().await;
    store
        .save_runtime_configuration(
            &app.app_id,
            Prod,
            &save_request(&app, "save", 0, "newpassword"),
        )
        .await
        .unwrap();
    let (op, context) = claim_inline(&store, &app, "preflight", "newpassword").await;
    store
        .bind_runtime_configuration_target(&context, 1, &target())
        .await
        .unwrap();
    store
        .record_runtime_configuration_result(&context, 1, &target(), C::Applying, B::NotStarted)
        .await
        .unwrap();
    // Executor confirmed it never dispatched ALTER (for example role missing).
    store
        .record_runtime_configuration_result(&context, 1, &target(), C::Failed, B::NotStarted)
        .await
        .unwrap();
    store
        .advance(&progress(&op, UserAppOperationState::Failed))
        .await
        .unwrap();
    let status = store
        .runtime_configuration_status(&app.app_id, &app.lifecycle_id, Prod)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.applied_version, None);
    assert_eq!(status.applying_operation_id, None);
    assert!(status.pending);
    let (_retry, capture) = claim_inline(&store, &app, "retry", "newpassword").await;
    assert_eq!(
        store
            .operation_runtime_configuration(&capture)
            .await
            .unwrap()
            .unwrap()
            .credentials,
        C::Captured
    );
}

#[tokio::test]
async fn save_is_configuration_only_and_replay_cannot_promote_an_old_version() {
    let (_dir, store, app) = database().await;
    let first = save_request(&app, "saveone", 0, "secret-one");
    let saved = store
        .save_runtime_configuration(&app.app_id, Prod, &first)
        .await
        .unwrap();
    assert_eq!(saved.config_version, 1);
    assert!(saved.status.pending);
    assert_eq!(saved.status.applied_version, None);
    assert_eq!(saved.status.applying_operation_id, None);
    assert_eq!(
        store.get_application(&app.app_id).await.unwrap().unwrap(),
        app
    );
    assert!(
        store
            .unfinished_operations(None, 50)
            .await
            .unwrap()
            .is_empty()
    );
    store
        .save_runtime_configuration(
            &app.app_id,
            Prod,
            &save_request(&app, "savetwo", 1, "secret-two"),
        )
        .await
        .unwrap();
    let replay = store
        .save_runtime_configuration(&app.app_id, Prod, &first)
        .await
        .unwrap();
    assert_eq!(replay.config_version, 1);
    assert_eq!(replay.status.saved_version, 2);
    let mut changed = first.clone();
    changed.pg.password = "changed".into();
    assert!(
        store
            .save_runtime_configuration(&app.app_id, Prod, &changed)
            .await
            .is_err()
    );
    assert!(
        store
            .save_runtime_configuration(&app.app_id, Prod, &save_request(&app, "stale", 0, "stale"))
            .await
            .is_err()
    );
    let public = serde_json::to_string(&replay).unwrap();
    assert!(!public.contains("secret"));
    assert!(!format!("{first:?}").contains("secret-one"));
    store.shutdown().await.unwrap();
}
#[tokio::test]
async fn explicit_capture_is_stable_but_ordinary_wake_and_restart_do_not_capture() {
    let (_dir, store, app) = database().await;
    store
        .save_runtime_configuration(&app.app_id, Prod, &save_request(&app, "first", 0, "one"))
        .await
        .unwrap();
    let (op, context) = claim_inline(&store, &app, "startone", "one").await;
    store
        .save_runtime_configuration(&app.app_id, Prod, &save_request(&app, "second", 1, "two"))
        .await
        .unwrap();
    let capture = store
        .operation_runtime_configuration(&context)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(capture.config_version, 1);
    assert_eq!(capture.pg.password, "one");
    ready(&store, &context, 1).await;
    let state = store
        .runtime_configuration_status(&app.app_id, &app.lifecycle_id, Prod)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.saved_version, 2);
    assert_eq!(state.applied_version, Some(1));
    assert!(state.pending);
    store
        .advance(&progress(&op, UserAppOperationState::Succeeded))
        .await
        .unwrap();
    let (wake, context) = claim(
        &store,
        &admission(
            &app,
            "trafficwake",
            UserAppControlCommand::Start { traffic: true },
        ),
    )
    .await;
    assert!(
        store
            .operation_runtime_configuration(&context)
            .await
            .unwrap()
            .is_none()
    );
    // No side effects yet: a confirmed failure does not activate version two.
    store
        .advance(&progress(&wake, UserAppOperationState::Failed))
        .await
        .unwrap();
    let (_, context) = claim(
        &store,
        &admission(&app, "explicitrestart", UserAppControlCommand::Restart),
    )
    .await;
    assert!(
        store
            .operation_runtime_configuration(&context)
            .await
            .unwrap()
            .is_none()
    );
    let unchanged = store
        .runtime_configuration_status(&app.app_id, &app.lifecycle_id, Prod)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.saved_version, 2);
    assert_eq!(unchanged.applied_version, Some(1));
    assert!(unchanged.pending);
    store.shutdown().await.unwrap();
}
#[tokio::test]
async fn save_after_admission_does_not_turn_original_absence_into_new_credentials() {
    let (_dir, store, app) = database().await;
    let request = admission(
        &app,
        "nocredentials",
        UserAppControlCommand::Start { traffic: false },
    );
    let (_, context) = claim(&store, &request).await;
    store
        .save_runtime_configuration(&app.app_id, Prod, &save_request(&app, "late", 0, "later"))
        .await
        .unwrap();
    store.admit(&request).await.unwrap();
    assert!(
        store
            .operation_runtime_configuration(&context)
            .await
            .unwrap()
            .is_none()
    );
    store.shutdown().await.unwrap();
}
#[tokio::test]
async fn saved_configuration_is_isolated_and_neither_scope_implicitly_captures() {
    let (_dir, store, app) = database().await;
    store
        .save_runtime_configuration(&app.app_id, Prod, &save_request(&app, "sameid", 0, "prod"))
        .await
        .unwrap();
    store
        .save_runtime_configuration(&app.app_id, Dev, &save_request(&app, "sameid", 0, "dev"))
        .await
        .unwrap();
    let (_, prod) = claim(
        &store,
        &admission(
            &app,
            "prodstart",
            UserAppControlCommand::Start { traffic: false },
        ),
    )
    .await;
    let (_, dev) = claim(
        &store,
        &admission(&app, "devrestart", UserAppControlCommand::RestartBuilder),
    )
    .await;
    for context in [&prod, &dev] {
        assert!(
            store
                .operation_runtime_configuration(context)
                .await
                .unwrap()
                .is_none()
        );
    }
    for scope in [Prod, Dev] {
        let status = store
            .runtime_configuration_status(&app.app_id, &app.lifecycle_id, scope)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.saved_version, 1);
        assert_eq!(status.applied_version, None);
        assert_eq!(status.applying_operation_id, None);
        assert!(status.pending);
    }
    let mut stale = save_request(&app, "stale", 1, "bad");
    stale.lifecycle_id = "otherlife".into();
    assert!(matches!(
        store
            .save_runtime_configuration(&app.app_id, Prod, &stale)
            .await,
        Err(UserAppStoreError::LifecycleConflict)
    ));
    store.shutdown().await.unwrap();
}
#[tokio::test]
async fn unknown_pg_write_cannot_be_finalized_or_rebound_and_business_failure_keeps_applied_version()
 {
    let (_dir, store, app) = database().await;
    store
        .save_runtime_configuration(&app.app_id, Prod, &save_request(&app, "first", 0, "secret"))
        .await
        .unwrap();
    let (op, context) = claim_inline(&store, &app, "startup", "secret").await;
    assert!(
        store
            .advance(&progress(&op, UserAppOperationState::Succeeded))
            .await
            .is_err()
    );
    let mut wrong = context.clone();
    wrong.executor_id = "otherexecutor".into();
    assert!(store.operation_runtime_configuration(&wrong).await.is_err());
    store
        .bind_runtime_configuration_target(&context, 1, &target())
        .await
        .unwrap();
    let mut changed = target();
    changed.physical_uid = "differentpod".into();
    assert!(
        store
            .bind_runtime_configuration_target(&context, 1, &changed)
            .await
            .is_err()
    );
    store
        .record_runtime_configuration_result(&context, 1, &target(), C::Applying, B::NotStarted)
        .await
        .unwrap();
    store
        .record_runtime_configuration_result(&context, 1, &target(), C::Unknown, B::NotStarted)
        .await
        .unwrap();
    assert!(
        store
            .record_runtime_configuration_result(&context, 1, &target(), C::Failed, B::Failed)
            .await
            .is_err()
    );
    assert!(
        store
            .advance(&progress(&op, UserAppOperationState::Failed))
            .await
            .is_err()
    );
    assert!(
        store
            .advance(&progress(&op, UserAppOperationState::Succeeded))
            .await
            .is_err()
    );
    let status = store
        .runtime_configuration_status(&app.app_id, &app.lifecycle_id, Prod)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.applying_operation_id.as_deref(), Some("startup"));
    assert_eq!(status.applied_version, None);
    // Exact operation can confirm its prior uncertain write with physical/TCP
    // evidence. It must not rewrite the target or downgrade unknown to failed.
    store
        .record_runtime_configuration_result(&context, 1, &target(), C::Applied, B::Starting)
        .await
        .unwrap();
    store
        .record_runtime_configuration_result(&context, 1, &target(), C::Applied, B::Failed)
        .await
        .unwrap();
    store
        .advance(&progress(&op, UserAppOperationState::Failed))
        .await
        .unwrap();
    let status = store
        .runtime_configuration_status(&app.app_id, &app.lifecycle_id, Prod)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.applied_version, Some(1));
    assert!(!status.pending);
    assert_eq!(status.applying_operation_id, None);
    assert!(
        store
            .operation_runtime_configuration(&context)
            .await
            .is_err()
    );
    store.shutdown().await.unwrap();
}
#[tokio::test]
async fn hot_deploy_ignores_pending_historical_credentials() {
    let (_dir, store, app) = database().await;
    store
        .save_runtime_configuration(&app.app_id, Prod, &save_request(&app, "first", 0, "secret"))
        .await
        .unwrap();
    let mut request = admission(
        &app,
        "hotdeploy",
        UserAppControlCommand::Start { traffic: false },
    );
    request.kind = UserAppOperationKind::HotDeploy;
    request.command = None;
    let (op, context) = claim(&store, &request).await;
    assert!(
        store
            .operation_runtime_configuration(&context)
            .await
            .unwrap()
            .is_none()
    );
    let current = store.get_application(&app.app_id).await.unwrap().unwrap();
    assert_eq!(
        current.active_operations.slot(Prod).unwrap(),
        &op.operation_id
    );
    let status = store
        .runtime_configuration_status(&app.app_id, &app.lifecycle_id, Prod)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.saved_version, 1);
    assert_eq!(status.applied_version, None);
    assert_eq!(status.applying_operation_id, None);
    assert!(status.pending);
    store
        .advance(&progress(&op, UserAppOperationState::Failed))
        .await
        .unwrap();
    assert!(
        store
            .get_application(&app.app_id)
            .await
            .unwrap()
            .unwrap()
            .active_operations
            .is_empty()
    );
    store.shutdown().await.unwrap();
}
#[tokio::test]
async fn concurrent_saves_at_one_revision_have_one_winner() {
    let (_dir, store, app) = database().await;
    let left = save_request(&app, "left", 0, "one");
    let right = save_request(&app, "right", 0, "two");
    let (a, b) = tokio::join!(
        store.save_runtime_configuration(&app.app_id, Prod, &left),
        store.save_runtime_configuration(&app.app_id, Prod, &right)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(matches!(
        a.as_ref().err().or(b.as_ref().err()),
        Some(UserAppStoreError::VersionConflict)
    ));
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn database_admin_accepts_managed_credentials_with_same_scope_isolation() {
    let (_directory, store, app) = database().await;
    store
        .save_runtime_configuration(&app.app_id, Prod, &save_request(&app, "saved", 0, "secret"))
        .await
        .unwrap();
    let request = admission(
        &app,
        "admin",
        UserAppControlCommand::ResetDatabasePassword {
            production: true,
            username: "business".into(),
        },
    );
    let (op, context) = claim(&store, &request).await;
    assert!(
        store
            .operation_runtime_configuration(&context)
            .await
            .unwrap()
            .is_none()
    );
    let current = store.get_application(&app.app_id).await.unwrap().unwrap();
    assert_eq!(
        current.active_operations.slot(Prod).unwrap(),
        &op.operation_id
    );
    let mut concurrent = request.clone();
    concurrent.operation_id = "concurrent".into();
    concurrent.request_id = Some("concurrent".into());
    assert!(
        matches!(store.admit(&concurrent).await, Err(UserAppStoreError::OperationInProgress(blocker)) if blocker.operation_id == op.operation_id)
    );
    let (_, dev) = claim(
        &store,
        &admission(
            &app,
            "devadmin",
            UserAppControlCommand::ResetDatabasePassword {
                production: false,
                username: "business".into(),
            },
        ),
    )
    .await;
    assert!(
        store
            .operation_runtime_configuration(&dev)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn database_admin_blocks_same_scope_configuration_until_confirmed_terminal() {
    let (_directory, store, app) = database().await;
    let request = admission(
        &app,
        "admin",
        UserAppControlCommand::ResetDatabasePassword {
            production: true,
            username: "independent".into(),
        },
    );
    let (op, _) = claim(&store, &request).await;
    let save = save_request(&app, "saved", 0, "secret");
    assert!(
        matches!(store.save_runtime_configuration(&app.app_id, Prod, &save).await,
        Err(UserAppStoreError::OperationInProgress(blocker)) if blocker.operation_id == op.operation_id)
    );
    // Environment isolation remains intact: a prod DB command cannot block dev saves.
    store
        .save_runtime_configuration(&app.app_id, Dev, &save)
        .await
        .unwrap();
    let uncertain = store
        .advance(&progress(&op, UserAppOperationState::RecoveryRequired))
        .await
        .unwrap();
    assert!(matches!(
        store
            .save_runtime_configuration(&app.app_id, Prod, &save)
            .await,
        Err(UserAppStoreError::OperationInProgress(_))
    ));
    assert_eq!(uncertain.operation_id, op.operation_id);
}

#[tokio::test]
async fn database_admin_allows_saved_and_applied_accounts_without_losing_slot_protection() {
    let (_directory, store, app) = database().await;
    let first = save_request(&app, "savedone", 0, "secret");
    store
        .save_runtime_configuration(&app.app_id, Prod, &first)
        .await
        .unwrap();
    let (op, context) = claim_inline(&store, &app, "startone", "secret").await;
    ready(&store, &context, 1).await;
    store
        .advance(&progress(&op, UserAppOperationState::Succeeded))
        .await
        .unwrap();
    let mut second = save_request(&app, "savedtwo", 1, "newsecret");
    second.pg.username = "nextbusiness".into();
    store
        .save_runtime_configuration(&app.app_id, Prod, &second)
        .await
        .unwrap();
    for (id, username) in [("oldadmin", "business"), ("newadmin", "nextbusiness")] {
        let request = admission(
            &app,
            id,
            UserAppControlCommand::ResetDatabasePassword {
                production: true,
                username: username.into(),
            },
        );
        let (admin, context) = claim(&store, &request).await;
        assert!(
            store
                .operation_runtime_configuration(&context)
                .await
                .unwrap()
                .is_none()
        );
        let blocked = admission(
            &app,
            "competingadmin",
            UserAppControlCommand::ResetDatabasePassword {
                production: true,
                username: "other".into(),
            },
        );
        assert!(
            matches!(store.admit(&blocked).await, Err(UserAppStoreError::OperationInProgress(blocker)) if blocker.operation_id == admin.operation_id)
        );
        // Confirmed pre-write failure releases this identity, not the next request.
        store
            .advance(&progress(&admin, UserAppOperationState::Failed))
            .await
            .unwrap();
    }
    let (admin, _) = claim(
        &store,
        &admission(
            &app,
            "independentadmin",
            UserAppControlCommand::ResetDatabasePassword {
                production: true,
                username: "independent".into(),
            },
        ),
    )
    .await;
    // A completed configuration save can be read/replayed during DB administration.
    assert_eq!(
        store
            .save_runtime_configuration(&app.app_id, Prod, &second)
            .await
            .unwrap()
            .config_version,
        2
    );
    store
        .advance(&progress(&admin, UserAppOperationState::Failed))
        .await
        .unwrap();
    // A confirmed pre-write rejection releases exactly this operation's slot.
    let third = save_request(&app, "savedthree", 2, "thirdsecret");
    assert_eq!(
        store
            .save_runtime_configuration(&app.app_id, Prod, &third)
            .await
            .unwrap()
            .config_version,
        3
    );
}

#[tokio::test]
async fn database_password_write_requires_ordered_evidence_and_retains_uncertain_slot() {
    let (_directory, store, app) = database().await;
    let (mut op, context) = claim(
        &store,
        &admission(
            &app,
            "passwordop",
            UserAppControlCommand::ResetDatabasePassword {
                production: true,
                username: "independent".into(),
            },
        ),
    )
    .await;
    assert!(
        store
            .advance(&progress(&op, UserAppOperationState::Succeeded))
            .await
            .is_err()
    );
    let mut evidence = DatabasePasswordEvidence {
        receipt_protocol: Some(1),
        context,
        username: "independent".into(),
        target: DatabasePasswordTarget::Prod(target()),
        stage: DatabasePasswordStage::Captured,
    };
    let checkpoint = |op: &UserAppOperationRecord, evidence: &DatabasePasswordEvidence| {
        let mut p = progress(op, UserAppOperationState::Running);
        p.checkpoint = serde_json::to_value(evidence).unwrap();
        p
    };
    op = store.advance(&checkpoint(&op, &evidence)).await.unwrap();
    evidence.stage = DatabasePasswordStage::Verified;
    assert!(store.advance(&checkpoint(&op, &evidence)).await.is_err());
    evidence.stage = DatabasePasswordStage::WriteSubmitted;
    op = store.advance(&checkpoint(&op, &evidence)).await.unwrap();
    let mut failed = progress(&op, UserAppOperationState::Failed);
    failed.checkpoint = serde_json::to_value(&evidence).unwrap();
    assert!(store.advance(&failed).await.is_err());
    let mut different = evidence.clone();
    different.target = DatabasePasswordTarget::Prod(RuntimeConfigurationTarget {
        physical_uid: "replacement".into(),
        deployment_generation: "newgeneration".into(),
    });
    assert!(store.advance(&checkpoint(&op, &different)).await.is_err());
    let mut recovery = progress(&op, UserAppOperationState::RecoveryRequired);
    recovery.checkpoint = serde_json::to_value(&evidence).unwrap();
    let uncertain = store.advance(&recovery).await.unwrap();
    assert!(!userapp_operation_has_final_evidence(&uncertain));
    let app = store.get_application(&app.app_id).await.unwrap().unwrap();
    assert_eq!(app.active_operations.slot(Prod), Some(&op.operation_id));
}

#[tokio::test]
async fn database_password_verified_evidence_allows_terminal_commit() {
    let (_directory, store, app) = database().await;
    let (mut op, context) = claim(
        &store,
        &admission(
            &app,
            "passwordop",
            UserAppControlCommand::ResetDatabasePassword {
                production: true,
                username: "independent".into(),
            },
        ),
    )
    .await;
    for stage in [
        DatabasePasswordStage::Captured,
        DatabasePasswordStage::WriteSubmitted,
        DatabasePasswordStage::Verified,
    ] {
        let evidence = DatabasePasswordEvidence {
            receipt_protocol: Some(1),
            context: context.clone(),
            username: "independent".into(),
            target: DatabasePasswordTarget::Prod(target()),
            stage,
        };
        let mut p = progress(&op, UserAppOperationState::Running);
        p.checkpoint = serde_json::to_value(evidence).unwrap();
        op = store.advance(&p).await.unwrap();
    }
    assert!(userapp_operation_has_final_evidence(&op));
    let mut p = progress(&op, UserAppOperationState::Succeeded);
    p.checkpoint = op.checkpoint.clone();
    let done = store.advance(&p).await.unwrap();
    assert_eq!(done.state, UserAppOperationState::Succeeded);
    assert!(
        store
            .get_application(&app.app_id)
            .await
            .unwrap()
            .unwrap()
            .active_operations
            .slot(Prod)
            .is_none()
    );
}

#[tokio::test]
async fn database_preparation_requires_ordered_evidence_and_keeps_runtime_policy() {
    for unknown in [false, true] {
        let (_directory, store, app) = database().await;
        let request = admission(
            &app,
            "prepareone",
            UserAppControlCommand::PrepareProdDatabase,
        );
        let (mut op, context) = claim(&store, &request).await;
        assert_eq!(op.scope, Prod);
        assert!(
            store
                .advance(&progress(&op, UserAppOperationState::Succeeded))
                .await
                .is_err()
        );
        let mut evidence = DatabasePreparationEvidence {
            target: UserAppMutationTarget {
                context,
                resource: AppResourceIdentity {
                    kind: AppResourceKind::Container,
                    name: "capturedcontainer".into(),
                    uid: "physicalone".into(),
                    resource_version: None,
                },
            },
            deployment_generation: "generationone".into(),
            stage: DatabasePreparationStage::Captured,
            management: None,
        };
        let mut update = progress(&op, UserAppOperationState::Running);
        update.checkpoint = serde_json::to_value(&evidence).unwrap();
        op = store.advance(&update).await.unwrap();
        // Neither forged Ready nor replacing the target can skip the persisted
        // start boundary, even though all records use the same operation ID.
        let mut forged = evidence.clone();
        forged.stage = DatabasePreparationStage::ManagementReady;
        forged.management = Some(target());
        update = progress(&op, UserAppOperationState::Running);
        update.checkpoint = serde_json::to_value(&forged).unwrap();
        assert!(store.advance(&update).await.is_err());
        evidence.stage = DatabasePreparationStage::StartSubmitted;
        update.checkpoint = serde_json::to_value(&evidence).unwrap();
        op = store.advance(&update).await.unwrap();
        update = progress(&op, UserAppOperationState::Failed);
        update.checkpoint = op.checkpoint.clone();
        assert!(store.advance(&update).await.is_err());
        forged.target.resource.uid = "replacement".into();
        update.state = UserAppOperationState::Running;
        update.checkpoint = serde_json::to_value(forged).unwrap();
        assert!(store.advance(&update).await.is_err());
        if unknown {
            update = progress(&op, UserAppOperationState::RecoveryRequired);
            update.checkpoint = op.checkpoint.clone();
            op = store.advance(&update).await.unwrap();
            assert!(!userapp_operation_has_final_evidence(&op));
            let current = store.get_application(&app.app_id).await.unwrap().unwrap();
            assert_eq!(
                current.active_operations.prod.as_deref(),
                Some("prepareone")
            );
        } else {
            evidence.stage = DatabasePreparationStage::ManagementReady;
            evidence.management = Some(target());
            update = progress(&op, UserAppOperationState::Running);
            update.checkpoint = serde_json::to_value(evidence).unwrap();
            op = store.advance(&update).await.unwrap();
            update = progress(&op, UserAppOperationState::Succeeded);
            update.checkpoint = op.checkpoint.clone();
            op = store.advance(&update).await.unwrap();
            assert!(userapp_operation_has_final_evidence(&op));
            let current = store.get_application(&app.app_id).await.unwrap().unwrap();
            assert!(current.active_operations.prod.is_none());
            assert_eq!(current.runtime_policy, app.runtime_policy);
        }
    }
}

#[tokio::test]
async fn password_recovery_finalization_cas_preserves_identity_and_lease() {
    for final_stage in [
        DatabasePasswordStage::Verified,
        DatabasePasswordStage::Cancelled,
    ] {
        let (_directory, store, app) = database().await;
        let (mut op, context) = claim(
            &store,
            &admission(
                &app,
                "recoverpassword",
                UserAppControlCommand::ResetDatabasePassword {
                    production: true,
                    username: "independent".into(),
                },
            ),
        )
        .await;
        let mut evidence = DatabasePasswordEvidence {
            receipt_protocol: Some(1),
            context: context.clone(),
            username: "independent".into(),
            target: DatabasePasswordTarget::Prod(target()),
            stage: DatabasePasswordStage::Captured,
        };
        for stage in [
            DatabasePasswordStage::Captured,
            DatabasePasswordStage::WriteSubmitted,
        ] {
            evidence.stage = stage;
            let mut p = progress(&op, UserAppOperationState::Running);
            p.checkpoint = serde_json::to_value(&evidence).unwrap();
            op = store.advance(&p).await.unwrap();
        }
        evidence.stage = final_stage.clone();
        assert!(
            store
                .finalize_password_recovery(&op, &evidence)
                .await
                .is_err(),
            "missing lease must not authorize finalization"
        );
        let receipt = UserAppOperationLeaseReceipt::Docker {
            service_type: ServiceType::Userapp,
            device: 1,
            inode: 42,
            token: "originallease".into(),
        };
        store
            .bind_operation_lease(&context, &receipt)
            .await
            .unwrap();
        let mut p = progress(&op, UserAppOperationState::RecoveryRequired);
        p.checkpoint = op.checkpoint.clone();
        op = store.advance(&p).await.unwrap();
        let mut legacy = evidence.clone();
        legacy.receipt_protocol = None;
        assert!(
            store
                .finalize_password_recovery(&op, &legacy)
                .await
                .is_err()
        );
        let mut wrong = evidence.clone();
        wrong.target = DatabasePasswordTarget::Prod(RuntimeConfigurationTarget {
            physical_uid: "replacement".into(),
            deployment_generation: "replacement".into(),
        });
        assert!(store.finalize_password_recovery(&op, &wrong).await.is_err());
        let mut stale = op.clone();
        stale.revision -= 1;
        assert!(
            store
                .finalize_password_recovery(&stale, &evidence)
                .await
                .is_err()
        );
        let done = store
            .finalize_password_recovery(&op, &evidence)
            .await
            .unwrap();
        assert_eq!(
            done.state,
            if final_stage == DatabasePasswordStage::Verified {
                UserAppOperationState::Succeeded
            } else {
                UserAppOperationState::Failed
            }
        );
        assert!(
            store
                .finalize_password_recovery(&op, &evidence)
                .await
                .is_err()
        );
        assert_eq!(done.operation_id, op.operation_id);
        assert_eq!(done.executor_id, op.executor_id);
        assert!(
            store
                .get_application(&app.app_id)
                .await
                .unwrap()
                .unwrap()
                .active_operations
                .slot(Prod)
                .is_none()
        );
        assert_eq!(
            store
                .get_operation_lease(&app.app_id, &op.operation_id)
                .await
                .unwrap()
                .unwrap()
                .receipt,
            receipt,
            "physical lease persists until exact cleanup"
        );
    }
}

#[tokio::test]
async fn deploy_pg_recovery_finalization_is_cas_identity_and_outcome_bound() {
    for (final_stage, fragment) in [
        (
            DatabasePasswordStage::Verified,
            "committed and verified; deployment completion was not recorded",
        ),
        (
            DatabasePasswordStage::Cancelled,
            "write was cancelled; deployment did not complete",
        ),
    ] {
        let (_directory, store, app) = database().await;
        let input = UserAppExecutionInput::new("{}".into());
        let request = admission(
            &app,
            "recoverdeploypg",
            UserAppControlCommand::Deploy {
                restart: false,
                input_digest: input.digest(),
            },
        );
        let (mut op, context) = claim_admitted(
            &store,
            match store
                .admit_with_configuration(&request, &input, None)
                .await
                .unwrap()
            {
                UserAppAdmissionOutcome::Accepted(op) | UserAppAdmissionOutcome::Existing(op) => op,
            },
        )
        .await;
        let mut evidence = ExplicitDeploymentPasswordEvidence {
            receipt_protocol: Some(1),
            context: context.clone(),
            username: "business".into(),
            explicit_pg_target: target(),
            stage: DatabasePasswordStage::WriteSubmitted,
        };
        let mut p = progress(&op, UserAppOperationState::Running);
        p.checkpoint = serde_json::to_value(&evidence).unwrap();
        op = store.advance(&p).await.unwrap();
        evidence.stage = final_stage.clone();
        assert!(
            store
                .finalize_deploy_pg_recovery(&op, &evidence)
                .await
                .is_err(),
            "missing lease must not authorize finalization"
        );
        let receipt = UserAppOperationLeaseReceipt::Docker {
            service_type: ServiceType::Userapp,
            device: 1,
            inode: 42,
            token: "originallease".into(),
        };
        store
            .bind_operation_lease(&context, &receipt)
            .await
            .unwrap();
        assert!(
            store
                .finalize_deploy_pg_recovery(&op, &evidence)
                .await
                .is_err(),
            "a matching lease and password receipt do not prove the running coordinator exited"
        );
        assert_eq!(
            store
                .get_operation(&app.app_id, &op.operation_id)
                .await
                .unwrap()
                .unwrap(),
            op
        );
        let mut p = progress(&op, UserAppOperationState::RecoveryRequired);
        p.checkpoint = op.checkpoint.clone();
        op = store.advance(&p).await.unwrap();
        let mut legacy = evidence.clone();
        legacy.receipt_protocol = None;
        assert!(
            store
                .finalize_deploy_pg_recovery(&op, &legacy)
                .await
                .is_err()
        );
        let mut wrong = evidence.clone();
        wrong.explicit_pg_target = RuntimeConfigurationTarget {
            physical_uid: "replacement".into(),
            deployment_generation: "replacement".into(),
        };
        assert!(
            store
                .finalize_deploy_pg_recovery(&op, &wrong)
                .await
                .is_err()
        );
        let mut stale = op.clone();
        stale.revision -= 1;
        assert!(
            store
                .finalize_deploy_pg_recovery(&stale, &evidence)
                .await
                .is_err()
        );
        // The original admission stays fenced until its own recovery finalizes.
        assert!(
            store
                .admit(&admission(
                    &app,
                    "seconddeploy",
                    UserAppControlCommand::Deploy {
                        restart: false,
                        input_digest: "c".repeat(64),
                    },
                ))
                .await
                .is_err()
        );
        let done = store
            .finalize_deploy_pg_recovery(&op, &evidence)
            .await
            .unwrap();
        assert_eq!(done.state, UserAppOperationState::Failed);
        assert!(
            done.error_message
                .as_deref()
                .is_some_and(|message| message.contains(fragment))
        );
        let stored: ExplicitDeploymentPasswordEvidence =
            serde_json::from_value(done.checkpoint.clone()).unwrap();
        assert_eq!(stored.stage, final_stage);
        assert!(
            store
                .finalize_deploy_pg_recovery(&op, &evidence)
                .await
                .is_err()
        );
        assert_eq!(done.operation_id, op.operation_id);
        assert_eq!(done.executor_id, op.executor_id);
        assert!(
            store
                .get_application(&app.app_id)
                .await
                .unwrap()
                .unwrap()
                .active_operations
                .slot(Prod)
                .is_none()
        );
        assert_eq!(
            store
                .get_operation_lease(&app.app_id, &op.operation_id)
                .await
                .unwrap()
                .unwrap()
                .receipt,
            receipt,
            "physical lease persists until exact cleanup"
        );
    }
}

#[tokio::test]
async fn inline_deployment_credentials_are_atomic_captured_and_replay_stable() {
    let (_directory, store, app) = database().await;
    let pg = StartPgCredential {
        username: "runtimeuser".into(),
        password: "originalprivate".into(),
    };
    let input = UserAppExecutionInput::new(serde_json::json!({"request":{"pg":pg}}).to_string());
    let request = admission(
        &app,
        "inlinefirst",
        UserAppControlCommand::Deploy {
            restart: false,
            input_digest: input.digest(),
        },
    );
    let accepted = store
        .admit_with_configuration(&request, &input, Some(&pg))
        .await
        .unwrap();
    let UserAppAdmissionOutcome::Accepted(pending) = accepted else {
        panic!("new operation");
    };
    let running = store
        .advance(&progress(&pending, UserAppOperationState::Running))
        .await
        .unwrap();
    let context = UserAppExecutionContext {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        operation_id: running.operation_id.clone(),
        executor_id: "configworker".into(),
        request_fingerprint: running.request_fingerprint.clone(),
    };
    let captured = store
        .operation_runtime_configuration(&context)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(captured.pg, pg);
    assert_eq!(captured.config_version, 1);
    let status = store
        .runtime_configuration_status(&app.app_id, &app.lifecycle_id, Prod)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.saved_version, 1);
    assert_eq!(status.applied_version, None);
    let newer = SaveRuntimeConfigurationRequest {
        lifecycle_id: app.lifecycle_id.clone(),
        request_id: "laterexplicit".into(),
        expected_revision: 1,
        pg: StartPgCredential {
            username: "runtimeuser".into(),
            password: "newerprivate".into(),
        },
    };
    store
        .save_runtime_configuration(&app.app_id, Prod, &newer)
        .await
        .unwrap();
    assert!(matches!(
        store
            .admit_with_configuration(&request, &input, Some(&pg))
            .await
            .unwrap(),
        UserAppAdmissionOutcome::Existing(_)
    ));
    assert_eq!(
        store
            .operation_runtime_configuration(&context)
            .await
            .unwrap()
            .unwrap()
            .pg,
        pg
    );
    assert_eq!(
        store
            .runtime_configuration_status(&app.app_id, &app.lifecycle_id, Prod)
            .await
            .unwrap()
            .unwrap()
            .saved_version,
        2
    );
    assert!(
        store
            .admit_with_configuration(&request, &input, Some(&newer.pg))
            .await
            .is_err(),
        "explicit replay cannot substitute a different captured credential"
    );
}

#[tokio::test]
async fn inline_deployment_credential_conflict_rolls_back_admission() {
    let (_directory, store, app) = database().await;
    store
        .save_runtime_configuration(
            &app.app_id,
            Prod,
            &save_request(&app, "savedfirst", 0, "savedprivate"),
        )
        .await
        .unwrap();
    let pg = StartPgCredential {
        username: "business".into(),
        password: "differentprivate".into(),
    };
    let input = UserAppExecutionInput::new(serde_json::json!({"request":{"pg":pg}}).to_string());
    let request = admission(
        &app,
        "rejectedinline",
        UserAppControlCommand::Deploy {
            restart: false,
            input_digest: input.digest(),
        },
    );
    assert!(
        store
            .admit_with_configuration(&request, &input, Some(&pg))
            .await
            .is_err()
    );
    assert!(
        store
            .get_operation(&app.app_id, &request.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_operation_by_request(&app.app_id, request.request_id.as_deref().unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_application(&app.app_id)
            .await
            .unwrap()
            .unwrap()
            .active_operations
            .slot(Prod)
            .is_none()
    );
    assert_eq!(
        store
            .runtime_configuration_status(&app.app_id, &app.lifecycle_id, Prod)
            .await
            .unwrap()
            .unwrap()
            .saved_version,
        1
    );
}

#[tokio::test]
async fn preparation_recovery_requires_exact_snapshot_and_retains_original_lease() {
    let (_directory, store, app) = database().await;
    let (mut op, context) = claim(
        &store,
        &admission(
            &app,
            "preparerecover",
            UserAppControlCommand::PrepareProdDatabase,
        ),
    )
    .await;
    let receipt = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::Userapp,
        device: 1,
        inode: 42,
        token: "originallease".into(),
    };
    store
        .bind_operation_lease(&context, &receipt)
        .await
        .unwrap();
    let mut evidence = DatabasePreparationEvidence {
        target: UserAppMutationTarget {
            context,
            resource: AppResourceIdentity {
                kind: AppResourceKind::Container,
                name: "captured".into(),
                uid: "physicalone".into(),
                resource_version: None,
            },
        },
        deployment_generation: "generationone".into(),
        stage: DatabasePreparationStage::Captured,
        management: None,
    };
    let mut update = progress(&op, UserAppOperationState::Running);
    update.checkpoint = serde_json::to_value(&evidence).unwrap();
    op = store.advance(&update).await.unwrap();
    let mut ready = evidence.clone();
    ready.stage = DatabasePreparationStage::ManagementReady;
    ready.management = Some(target());
    assert!(
        store
            .confirm_database_preparation_recovery(&op, &ready)
            .await
            .is_err()
    );
    evidence.stage = DatabasePreparationStage::StartSubmitted;
    update = progress(&op, UserAppOperationState::Running);
    update.checkpoint = serde_json::to_value(&evidence).unwrap();
    op = store.advance(&update).await.unwrap();
    let stale = op.clone();
    update = progress(&op, UserAppOperationState::RecoveryRequired);
    update.checkpoint = op.checkpoint.clone();
    op = store.advance(&update).await.unwrap();
    assert!(
        store
            .confirm_database_preparation_recovery(&stale, &ready)
            .await
            .is_err()
    );
    let mut wrong = ready.clone();
    wrong.target.resource.uid = "other".into();
    assert!(
        store
            .confirm_database_preparation_recovery(&op, &wrong)
            .await
            .is_err()
    );
    let confirmed = store
        .confirm_database_preparation_recovery(&op, &ready)
        .await
        .unwrap();
    assert_eq!(confirmed.executor_id, op.executor_id);
    assert!(userapp_operation_has_final_evidence(&confirmed));
    assert_eq!(confirmed.state, UserAppOperationState::Running);
    assert!(
        store
            .confirm_database_preparation_recovery(&op, &ready)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .get_operation_lease(&app.app_id, &op.operation_id)
            .await
            .unwrap()
            .unwrap()
            .receipt,
        receipt
    );
    let current = store.get_application(&app.app_id).await.unwrap().unwrap();
    assert_eq!(current.active_operations.prod, Some(op.operation_id));
    assert_eq!(current.runtime_policy, app.runtime_policy);
}
