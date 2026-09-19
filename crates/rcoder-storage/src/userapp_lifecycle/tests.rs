use super::*;
use shared_types::{
    UserAppAdmission, UserAppAdmissionOutcome as Outcome, UserAppLifecycleState,
    UserAppLifecycleStore, UserAppOperationKind as Kind, UserAppOperationProgress,
    UserAppOperationRecord, UserAppOperationState as State, UserAppStoreError as Error,
};

fn request(id: &str, kind: Kind) -> UserAppAdmission {
    UserAppAdmission {
        runtime_policy_on_success: None,
        command: None,
        metadata: None,
        app_id: "contract-app".into(),
        lifecycle_id: None,
        operation_id: id.into(),
        request_id: Some(id.into()),
        request_fingerprint: "a".repeat(64),
        kind,
    }
}
fn operation(outcome: Outcome) -> UserAppOperationRecord {
    match outcome {
        Outcome::Accepted(op) | Outcome::Existing(op) => op,
    }
}
fn progress(op: &UserAppOperationRecord, state: State) -> UserAppOperationProgress {
    UserAppOperationProgress {
        app_id: op.app_id.clone(),
        operation_id: op.operation_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        expected_revision: op.revision,
        executor_id: "worker-A".into(),
        state,
        step: "verified".into(),
        checkpoint: serde_json::json!({"uid":"physical-A"}),
        error_code: None,
        error_message: None,
    }
}
async fn database() -> (tempfile::TempDir, TursoUserAppStore) {
    let directory = tempfile::tempdir().unwrap();
    let path = std::path::absolute(directory.path().join("userapp.turso.db")).unwrap();
    let store = TursoUserAppStore::open_exclusive(&path).await.unwrap();
    (directory, store)
}

async fn storage_deletion_preserves_lifecycle(store: &dyn UserAppLifecycleStore) {
    let mut req = request("storage-only", Kind::DestroyProdStorage);
    req.app_id = "storage-only-app".into();
    let op = operation(store.admit(&req).await.unwrap());
    let before = store.get_application(&req.app_id).await.unwrap().unwrap();
    assert_eq!(before.state, UserAppLifecycleState::Active);
    complete(store, &op, State::Succeeded).await;
    let after = store.get_application(&req.app_id).await.unwrap().unwrap();
    assert_eq!(after.lifecycle_id, before.lifecycle_id);
    assert_eq!(after.state, UserAppLifecycleState::Active);
    assert!(after.active_operations.is_empty());
}

async fn admission_metadata_is_atomic(store: &dyn UserAppLifecycleStore) {
    let app = store.ensure_identity("atomic-admission-app").await.unwrap();
    let mut req = request("atomic-admission", Kind::Update);
    req.app_id = app.app_id.clone();
    req.lifecycle_id = Some(app.lifecycle_id.clone());
    req.metadata = Some(shared_types::UserAppMetadataPatch {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        expected_revision: app.metadata_revision,
        name: Some(Some("admitted-name".into())),
        tenant_id: Some(None),
        space_id: None,
    });
    let mut stale = req.clone();
    stale.metadata.as_mut().unwrap().expected_revision += 1;
    assert!(matches!(
        store.admit(&stale).await,
        Err(Error::VersionConflict)
    ));
    assert_eq!(
        store.get_application(&app.app_id).await.unwrap().unwrap(),
        app
    );
    assert!(
        store
            .get_operation(&app.app_id, &req.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    let op = operation(store.admit(&req).await.unwrap());
    let committed = store.get_application(&app.app_id).await.unwrap().unwrap();
    assert_eq!(committed.name.as_deref(), Some("admitted-name"));
    assert_eq!(committed.metadata_revision, app.metadata_revision + 1);
    assert_eq!(
        committed
            .active_operations
            .slot(op.scope)
            .map(String::as_str),
        Some(op.operation_id.as_str())
    );
    assert_eq!(op.admitted_metadata, req.metadata);
    assert!(
        matches!(store.admit(&req).await.unwrap(), Outcome::Existing(_)),
        "exact replay must not reapply stale metadata revision"
    );
    let mut changed = req.clone();
    changed.metadata.as_mut().unwrap().name = Some(Some("changed-with-same-token".into()));
    assert!(matches!(
        store.admit(&changed).await,
        Err(Error::InvalidOperation(_))
    ));
    changed.request_id = Some("competing-update".into());
    changed.operation_id = "competing-update".into();
    assert!(matches!(
        store.admit(&changed).await,
        Err(Error::OperationInProgress(_))
    ));
    assert_eq!(
        store.get_application(&app.app_id).await.unwrap().unwrap(),
        committed
    );
    complete(store, &op, State::Succeeded).await;
}

#[tokio::test]
async fn metadata_changes_commit_with_admission_and_never_before_rejection() {
    let (_directory, store) = database().await;
    admission_metadata_is_atomic(&store).await;
}

// 原子性反例的中途存储失败注入已由 Turso 后端原位覆盖
// （`common::local_tests::failed_admission_rolls_back_every_row_and_same_request_can_retry`）。

async fn request_identity_spans_recreation_and_control(store: &dyn UserAppLifecycleStore) {
    for (index, bad_request_id) in [" ".to_owned(), "a".repeat(129)].into_iter().enumerate() {
        let mut invalid = request("invalid-token", Kind::EnsureBuilder);
        invalid.app_id = format!("invalid-request-{index}");
        invalid.request_id = Some(bad_request_id);
        assert!(matches!(
            store.admit(&invalid).await,
            Err(Error::InvalidOperation(_))
        ));
        assert!(
            store
                .get_application(&invalid.app_id)
                .await
                .unwrap()
                .is_none(),
            "rejected admission must not persist identity"
        );
    }
    let mut invalid = request("invalid/operation", Kind::EnsureBuilder);
    invalid.app_id = "invalid-operation-token".into();
    assert!(matches!(
        store.admit(&invalid).await,
        Err(Error::InvalidOperation(_))
    ));
    assert!(
        store
            .get_application(&invalid.app_id)
            .await
            .unwrap()
            .is_none()
    );
    let mut deletion = request("request-space-delete", Kind::DeleteApplication);
    deletion.app_id = "request-space-app".into();
    let op = operation(store.admit(&deletion).await.unwrap());
    complete(store, &op, State::Succeeded).await;
    assert!(matches!(
        store
            .recreate(&deletion.app_id, &op.lifecycle_id, "request-space-delete")
            .await,
        Err(Error::InvalidOperation(_))
    ));
    let recreated = store
        .recreate(&deletion.app_id, &op.lifecycle_id, "request-space-recreate")
        .await
        .unwrap();
    let mut changed = request("request-space-start", Kind::Start);
    changed.app_id = deletion.app_id.clone();
    changed.lifecycle_id = Some(recreated.lifecycle_id.clone());
    changed.request_id = Some("request-space-recreate".into());
    assert!(matches!(
        store.admit(&changed).await,
        Err(Error::InvalidOperation(_))
    ));
    assert_eq!(
        store
            .get_application(&deletion.app_id)
            .await
            .unwrap()
            .unwrap(),
        recreated
    );
    assert_eq!(
        store
            .recreate(&deletion.app_id, &op.lifecycle_id, "request-space-recreate")
            .await
            .unwrap(),
        recreated
    );
}

#[tokio::test]
async fn recreation_and_control_cannot_reuse_the_same_request_identity() {
    let (_directory, store) = database().await;
    request_identity_spans_recreation_and_control(&store).await;
}

#[tokio::test]
async fn storage_deletion_does_not_end_the_application_lifecycle() {
    let (_directory, store) = database().await;
    storage_deletion_preserves_lifecycle(&store).await;
}

async fn paginated_scan_and_identity_contract(store: &dyn UserAppLifecycleStore) {
    let original = store.ensure_identity("scanidentity").await.unwrap();
    let patched = store
        .patch_metadata(&shared_types::UserAppMetadataPatch {
            app_id: original.app_id.clone(),
            lifecycle_id: original.lifecycle_id.clone(),
            expected_revision: original.metadata_revision,
            name: Some(Some("original".into())),
            tenant_id: Some(Some("tenant".into())),
            space_id: None,
        })
        .await
        .unwrap();
    assert_eq!(patched.created_at, original.created_at);
    assert_eq!(
        patched,
        store.ensure_identity(&original.app_id).await.unwrap()
    );
    let mut delete = request("scandelete", Kind::DeleteApplication);
    delete.app_id = original.app_id.clone();
    let op = operation(store.admit(&delete).await.unwrap());
    complete(store, &op, State::Succeeded).await;
    assert!(matches!(
        store.ensure_identity(&original.app_id).await,
        Err(Error::LifecycleConflict)
    ));
    let tombstone = store
        .get_application(&original.app_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tombstone.state, UserAppLifecycleState::Deleted);
    assert_eq!(tombstone.lifecycle_id, original.lifecycle_id);

    let mut expected = Vec::new();
    for i in 0..5 {
        let mut req = request(&format!("scan-{i}"), Kind::EnsureBuilder);
        req.app_id = format!("scan-app-{i}");
        let op = operation(store.admit(&req).await.unwrap());
        expected.push(op.operation_id.clone());
        if i == 0 {
            // A permanently unresolved first row must not starve later pages.
            complete(store, &op, State::RecoveryRequired).await;
        }
    }
    let mut cursor = None;
    let mut operations = Vec::new();
    loop {
        let page = store
            .unfinished_operations(cursor.as_deref(), 2)
            .await
            .unwrap();
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|op| op.operation_id.clone());
        operations.extend(page.into_iter().map(|op| op.operation_id));
    }
    assert_eq!(operations, expected);
    let mut cursor = None;
    let mut apps = Vec::new();
    loop {
        let page = store.list_applications(cursor.as_deref(), 2).await.unwrap();
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|app| app.app_id.clone());
        apps.extend(page.into_iter().map(|app| app.app_id));
    }
    assert!(apps.contains(&original.app_id));
    assert!(apps.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        apps.iter().filter(|id| id.starts_with("scan-app-")).count(),
        5
    );
    assert!(store.list_applications(None, 0).await.is_err());
    assert!(store.unfinished_operations(None, 0).await.is_err());
}

#[tokio::test]
async fn turso_paginated_recovery_and_identity() {
    let (_directory, store) = database().await;
    paginated_scan_and_identity_contract(&store).await;
}

async fn control_command_is_durable_and_part_of_deduplication(store: &dyn UserAppLifecycleStore) {
    store
        .ensure_identity("control-command")
        .await
        .expect("identity");
    let mut input = request("control-command-start", Kind::Start);
    input.app_id = "control-command".into();
    input.command = Some(shared_types::UserAppControlCommand::Start { traffic: false });
    let admitted = operation(store.admit(&input).await.expect("admission"));
    assert_eq!(admitted.command, input.command);
    assert_eq!(
        store
            .get_operation(&input.app_id, &input.operation_id)
            .await
            .expect("read")
            .expect("operation"),
        admitted
    );
    assert!(matches!(
        store.admit(&input).await.expect("repeat"),
        Outcome::Existing(_)
    ));
    let mut changed = input.clone();
    changed.command = Some(shared_types::UserAppControlCommand::Start { traffic: true });
    assert!(
        matches!(store.admit(&changed).await, Err(Error::InvalidOperation(_))),
        "equal caller keys and digests cannot hide a changed trigger"
    );
    changed.command = Some(shared_types::UserAppControlCommand::Stop {
        wake_on_traffic: true,
    });
    assert!(
        matches!(store.admit(&changed).await, Err(Error::InvalidOperation(_))),
        "command kind must match its operation"
    );
    assert_eq!(
        store
            .get_operation(&input.app_id, &input.operation_id)
            .await
            .expect("read")
            .expect("operation"),
        admitted
    );
    let mut legacy = serde_json::to_value(&admitted).expect("encoded record");
    legacy.as_object_mut().expect("object").remove("command");
    let decoded: UserAppOperationRecord = serde_json::from_value(legacy).expect("legacy record");
    assert!(
        decoded.command.is_none(),
        "missing replay input must remain missing"
    );
    complete(store, &admitted, State::Succeeded).await;
}

#[tokio::test]
async fn turso_control_command_persistence_contract() {
    let (_directory, store) = database().await;
    control_command_is_durable_and_part_of_deduplication(&store).await;
}

async fn runtime_policy_commits_only_with_success(store: &dyn UserAppLifecycleStore) {
    for (label, terminal) in [
        ("success", State::Succeeded),
        ("failure", State::Failed),
        ("unknown", State::RecoveryRequired),
    ] {
        let app_id = format!("policy-{label}");
        let before = store.ensure_identity(&app_id).await.expect("identity");
        let policy = shared_types::UserAppRuntimePolicy {
            recycle_enabled: Some(false),
            idle_timeout_seconds: Some(0),
            wake_on_traffic: Some(false),
        };
        let mut input = request(&format!("policy-command-{label}"), Kind::SetRecyclePolicy);
        input.app_id = app_id.clone();
        input.command = Some(shared_types::UserAppControlCommand::SetRecyclePolicy {
            policy: policy.clone(),
        });
        let admitted = operation(store.admit(&input).await.expect("policy admission"));
        assert_eq!(
            store
                .get_application(&app_id)
                .await
                .expect("read")
                .expect("app")
                .runtime_policy,
            before.runtime_policy
        );
        complete(store, &admitted, terminal).await;
        let after = store
            .get_application(&app_id)
            .await
            .expect("read")
            .expect("app");
        if terminal == State::Succeeded {
            assert_eq!(after.runtime_policy, policy);
            assert_eq!(after.metadata_revision, before.metadata_revision + 1);
            input.operation_id = "policy-same-content".into();
            input.request_id = Some("policy-same-content".into());
            let duplicate_content =
                operation(store.admit(&input).await.expect("same policy new request"));
            complete(store, &duplicate_content, State::Succeeded).await;
            assert_eq!(
                store
                    .get_application(&app_id)
                    .await
                    .expect("read")
                    .expect("app")
                    .metadata_revision,
                after.metadata_revision
            );
            let mut deletion = request("policy-delete", Kind::DeleteApplication);
            deletion.app_id = app_id.clone();
            complete(
                store,
                &operation(store.admit(&deletion).await.expect("delete")),
                State::Succeeded,
            )
            .await;
            let recreated = store
                .recreate(&app_id, &before.lifecycle_id, "policy-recreate")
                .await
                .expect("recreate");
            assert_eq!(
                recreated.runtime_policy,
                shared_types::UserAppRuntimePolicy::default()
            );
        } else {
            assert_eq!(after.runtime_policy, before.runtime_policy);
            assert_eq!(after.metadata_revision, before.metadata_revision);
        }
    }
}

async fn configuration_policy_is_transactional(store: &dyn UserAppLifecycleStore) {
    for (kind_label, kind) in [("create", Kind::Create), ("update", Kind::Update)] {
        for (label, terminal) in [
            ("ok", State::Succeeded),
            ("failed", State::Failed),
            ("unknown", State::RecoveryRequired),
        ] {
            let app_id = format!("config-policy-{kind_label}-{label}");
            store.ensure_identity(&app_id).await.expect("identity");
            let original = shared_types::UserAppRuntimePolicy {
                recycle_enabled: Some(false),
                idle_timeout_seconds: Some(600),
                wake_on_traffic: Some(false),
            };
            let mut seed = request(&format!("{app_id}-seed-policy"), Kind::SetRecyclePolicy);
            seed.app_id = app_id.clone();
            seed.command = Some(shared_types::UserAppControlCommand::SetRecyclePolicy {
                policy: original.clone(),
            });
            complete(
                store,
                &operation(store.admit(&seed).await.expect("seed")),
                State::Succeeded,
            )
            .await;
            let before = store
                .get_application(&app_id)
                .await
                .expect("read")
                .expect("app");
            let target = shared_types::UserAppRuntimePolicy {
                recycle_enabled: Some(true),
                idle_timeout_seconds: Some(1200),
                wake_on_traffic: None,
            };
            let mut input = request(&format!("{app_id}-configuration"), kind);
            input.app_id = app_id.clone();
            input.runtime_policy_on_success = Some(target.clone());
            let admitted = operation(store.admit(&input).await.expect("admit configuration"));
            let persisted = store
                .get_operation(&app_id, &admitted.operation_id)
                .await
                .expect("read operation")
                .expect("operation");
            assert_eq!(persisted.runtime_policy_on_success, Some(target));
            assert_eq!(
                store
                    .get_application(&app_id)
                    .await
                    .expect("read")
                    .expect("app")
                    .runtime_policy,
                original
            );
            let mut conflicting = input.clone();
            conflicting
                .runtime_policy_on_success
                .as_mut()
                .expect("policy")
                .idle_timeout_seconds = Some(99);
            assert!(matches!(
                store.admit(&conflicting).await,
                Err(Error::InvalidOperation(_))
            ));
            complete(store, &admitted, terminal).await;
            let after = store
                .get_application(&app_id)
                .await
                .expect("read")
                .expect("app");
            if terminal == State::Succeeded {
                assert_eq!(after.runtime_policy.recycle_enabled, Some(true));
                assert_eq!(after.runtime_policy.idle_timeout_seconds, Some(1200));
                assert_eq!(
                    after.runtime_policy.wake_on_traffic,
                    Some(false),
                    "omitted fields retain their applied value"
                );
                assert_eq!(after.metadata_revision, before.metadata_revision + 1);
            } else {
                assert_eq!(after.runtime_policy, original);
                assert_eq!(after.metadata_revision, before.metadata_revision);
            }
        }
    }
    let mut invalid = request("policy-wrong-kind", Kind::EnsureBuilder);
    invalid.app_id = "config-policy-wrong-kind".into();
    store
        .ensure_identity(&invalid.app_id)
        .await
        .expect("identity");
    invalid.runtime_policy_on_success = Some(Default::default());
    assert!(matches!(
        store.admit(&invalid).await,
        Err(Error::InvalidOperation(_))
    ));
}

#[tokio::test]
async fn turso_configuration_policy_contract() {
    let (_directory, store) = database().await;
    configuration_policy_is_transactional(&store).await;
}

#[tokio::test]
async fn turso_policy_commit_contract() {
    let (_directory, store) = database().await;
    runtime_policy_commits_only_with_success(&store).await;
}

#[tokio::test]
async fn turso_instance_directory_is_exclusive_and_restart_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("userapp.turso.db");
    let first = TursoUserAppStore::open_exclusive(&path).await.unwrap();
    let before = first.ensure_identity("persistent-app").await.unwrap();
    assert!(TursoUserAppStore::open_exclusive(&path).await.is_err());
    // A second database filename cannot hide sharing the same instance directory.
    assert!(
        TursoUserAppStore::open_exclusive(
            &std::path::absolute(dir.path().join("other.turso.db")).unwrap()
        )
        .await
        .is_err()
    );
    first.shutdown().await.expect("shutdown");
    drop(first);
    let restarted = TursoUserAppStore::open_exclusive(&path).await.unwrap();
    assert_eq!(
        before,
        restarted
            .get_application("persistent-app")
            .await
            .unwrap()
            .unwrap()
    );
    restarted.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn repeated_registration_is_an_idempotent_noop() {
    let (_directory, store) = database().await;
    let first = store.ensure_identity("contract-app").await.unwrap();
    assert_eq!(first, store.ensure_identity("contract-app").await.unwrap());
}

#[tokio::test]
async fn concurrent_ensure_joins_but_different_intent_does_not() {
    let (_directory, store) = database().await;
    let first = request("first", Kind::EnsureBuilder);
    let second = request("second", Kind::EnsureBuilder);
    let (a, b) = tokio::join!(store.admit(&first), store.admit(&second));
    let a = a.unwrap();
    let b = b.unwrap();
    assert!(matches!(
        (&a, &b),
        (Outcome::Accepted(_), Outcome::Existing(_)) | (Outcome::Existing(_), Outcome::Accepted(_))
    ));
    assert_eq!(operation(a).operation_id, operation(b).operation_id);
    assert!(matches!(
        store
            .admit(&request("delete", Kind::DeleteApplication))
            .await,
        Err(Error::OperationInProgress(_))
    ));
}

#[tokio::test]
async fn old_progress_cannot_overwrite_new_checkpoint() {
    let (_directory, store) = database().await;
    let op = operation(
        store
            .admit(&request("first", Kind::EnsureBuilder))
            .await
            .unwrap(),
    );
    let p = progress(&op, State::Running);
    let (a, b) = tokio::join!(store.advance(&p), store.advance(&p));
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert!(matches!(a.err().or(b.err()), Some(Error::VersionConflict)));
}

#[tokio::test]
async fn request_replay_returns_original_and_rejects_changed_parameters() {
    let (_directory, store) = database().await;
    let req = request("first", Kind::EnsureBuilder);
    let op = operation(store.admit(&req).await.unwrap());
    complete(&store, &op, State::Succeeded).await;
    let mut replay = req.clone();
    replay.operation_id = "new-network-attempt".into();
    let old = operation(store.admit(&replay).await.unwrap());
    assert_eq!(old.operation_id, op.operation_id);
    assert_eq!(old.state, State::Succeeded);
    replay.request_fingerprint = "config-B".into();
    assert!(matches!(
        store.admit(&replay).await,
        Err(Error::InvalidOperation(_))
    ));
}

#[tokio::test]
async fn deletion_and_recreation_fence_late_unqualified_requests() {
    let (_directory, store) = database().await;
    let op = operation(
        store
            .admit(&request("delete", Kind::DeleteApplication))
            .await
            .unwrap(),
    );
    assert_eq!(
        store
            .get_application("contract-app")
            .await
            .unwrap()
            .unwrap()
            .state,
        UserAppLifecycleState::Deleting
    );
    assert!(matches!(
        store.ensure_identity("contract-app").await,
        Err(Error::LifecycleConflict)
    ));
    complete(&store, &op, State::Succeeded).await;
    assert!(matches!(
        store.ensure_identity("contract-app").await,
        Err(Error::LifecycleConflict)
    ));
    let new = store
        .recreate("contract-app", &op.lifecycle_id, "recreate-1")
        .await
        .unwrap();
    assert_eq!(new.lifecycle_epoch, 2);
    assert_ne!(new.lifecycle_id, op.lifecycle_id);
    assert_eq!(
        new,
        store
            .recreate("contract-app", &op.lifecycle_id, "recreate-1")
            .await
            .unwrap()
    );
    assert!(matches!(
        store.admit(&request("late", Kind::Start)).await,
        Err(Error::LifecycleConflict)
    ));
    let mut qualified = request("qualified", Kind::Start);
    qualified.lifecycle_id = Some(new.lifecycle_id);
    assert!(matches!(
        store.admit(&qualified).await.unwrap(),
        Outcome::Accepted(_)
    ));
    assert!(matches!(
        store.advance(&progress(&op, State::Succeeded)).await,
        Err(Error::LifecycleConflict)
    ));
}

#[tokio::test]
async fn restart_keeps_operation_and_uncertainty_blocks_new_creators() {
    let (directory, store) = database().await;
    let op = operation(
        store
            .admit(&request("first", Kind::EnsureBuilder))
            .await
            .unwrap(),
    );
    complete(&store, &op, State::RecoveryRequired).await;
    store.shutdown().await.expect("shutdown");
    drop(store); // 目录锁随结构体持有——释放后才能重开
    let restored = TursoUserAppStore::open_exclusive(&directory.path().join("userapp.turso.db"))
        .await
        .unwrap();
    let unfinished = restored.unfinished_operations(None, 10).await.unwrap();
    assert_eq!(unfinished.len(), 1);
    assert_eq!(unfinished[0].operation_id, op.operation_id);
    assert_eq!(unfinished[0].state, State::RecoveryRequired);
    assert!(matches!(
        restored
            .admit(&request("second", Kind::EnsureBuilder))
            .await,
        Err(Error::OperationInProgress(_))
    ));
    restored.shutdown().await.expect("shutdown");
}

/// 反例 1（spec §7.1/7.3）：prod 未知结果（RecoveryRequired）不得阻塞 dev 域
/// 受理；同域排他与整体操作保护保留；prod 记录原样不动。
async fn cross_scope_admission_is_independent(store: &dyn UserAppLifecycleStore) {
    let app = store
        .ensure_identity("scope-cross-app")
        .await
        .expect("identity");
    let mut prod = request("scope-prod-wake", Kind::Start);
    prod.app_id = app.app_id.clone();
    prod.lifecycle_id = Some(app.lifecycle_id.clone());
    let prod_op = operation(store.admit(&prod).await.expect("prod admission"));
    complete(store, &prod_op, State::RecoveryRequired).await;
    let uncertain = store
        .get_operation(&app.app_id, &prod_op.operation_id)
        .await
        .expect("read")
        .expect("prod record");

    let mut dev = request("scope-dev-restart", Kind::RestartBuilder);
    dev.app_id = app.app_id.clone();
    dev.lifecycle_id = Some(app.lifecycle_id.clone());
    let dev_op = operation(
        store
            .admit(&dev)
            .await
            .expect("dev restart must be admitted while prod outcome is unknown"),
    );
    assert_eq!(dev_op.kind, Kind::RestartBuilder);

    let mut same_scope = request("scope-dev-stop", Kind::StopBuilder);
    same_scope.app_id = app.app_id.clone();
    same_scope.lifecycle_id = Some(app.lifecycle_id.clone());
    assert!(
        matches!(
            store.admit(&same_scope).await,
            Err(Error::OperationInProgress(_))
        ),
        "same-scope exclusivity must survive scope isolation"
    );

    let mut prod_again = request("scope-prod-stop", Kind::Stop);
    prod_again.app_id = app.app_id.clone();
    prod_again.lifecycle_id = Some(app.lifecycle_id.clone());
    assert!(
        matches!(
            store.admit(&prod_again).await,
            Err(Error::OperationInProgress(_))
        ),
        "prod slot stays occupied by its unresolved operation"
    );

    let mut whole = request("scope-delete", Kind::DeleteApplication);
    whole.app_id = app.app_id.clone();
    whole.lifecycle_id = Some(app.lifecycle_id.clone());
    assert!(
        matches!(
            store.admit(&whole).await,
            Err(Error::OperationInProgress(_))
        ),
        "application-wide operations require both environments idle"
    );

    assert_eq!(
        store
            .get_operation(&app.app_id, &prod_op.operation_id)
            .await
            .expect("read")
            .expect("prod record"),
        uncertain,
        "dev admission must not disturb the unresolved prod operation"
    );
}

/// 反例 2（spec §7.3）：dev 未知结果不得阻塞 prod 域受理。
async fn dev_uncertainty_does_not_block_prod(store: &dyn UserAppLifecycleStore) {
    let app = store
        .ensure_identity("scope-reverse-app")
        .await
        .expect("identity");
    let mut dev = request("reverse-dev-ensure", Kind::EnsureBuilder);
    dev.app_id = app.app_id.clone();
    dev.lifecycle_id = Some(app.lifecycle_id.clone());
    let dev_op = operation(store.admit(&dev).await.expect("dev admission"));
    complete(store, &dev_op, State::RecoveryRequired).await;

    let mut prod = request("reverse-prod-start", Kind::Start);
    prod.app_id = app.app_id.clone();
    prod.lifecycle_id = Some(app.lifecycle_id.clone());
    let prod_op = operation(
        store
            .admit(&prod)
            .await
            .expect("prod start must be admitted while dev outcome is unknown"),
    );
    assert_eq!(prod_op.kind, Kind::Start);

    let mut dev_again = request("reverse-dev-second", Kind::RestartBuilder);
    dev_again.app_id = app.app_id.clone();
    dev_again.lifecycle_id = Some(app.lifecycle_id.clone());
    assert!(matches!(
        store.admit(&dev_again).await,
        Err(Error::OperationInProgress(_))
    ));
}

#[tokio::test]
async fn turso_cross_scope_admission_is_independent() {
    let (_directory, store) = database().await;
    cross_scope_admission_is_independent(&store).await;
}

#[tokio::test]
async fn turso_dev_uncertainty_does_not_block_prod() {
    let (_directory, store) = database().await;
    dev_uncertainty_does_not_block_prod(&store).await;
}

/// §7.5/7.6：双域在途各自终态只清己槽；全部收束后整体操作才可受理且
/// Deleting 同事务生效；冲突错误携带结构化 blocker。
#[tokio::test]
async fn scoped_terminals_clear_own_slots_and_blocker_is_structured() {
    let (_directory, store) = database().await;
    let app = store.ensure_identity("scope-terminal-app").await.unwrap();
    let mut dev = request("terminal-dev", Kind::RestartBuilder);
    dev.app_id = app.app_id.clone();
    dev.lifecycle_id = Some(app.lifecycle_id.clone());
    let dev_op = operation(store.admit(&dev).await.unwrap());
    let mut prod = request("terminal-prod", Kind::Start);
    prod.app_id = app.app_id.clone();
    prod.lifecycle_id = Some(app.lifecycle_id.clone());
    prod.command = Some(shared_types::UserAppControlCommand::Start { traffic: true });
    prod.request_fingerprint = "b".repeat(64);
    let prod_op = operation(store.admit(&prod).await.unwrap());

    // prod 已受理未认领即占槽：同域第二个操作被拦，且 blocker 指名 prod 域
    let mut prod_again = request("terminal-prod-2", Kind::Stop);
    prod_again.app_id = app.app_id.clone();
    prod_again.lifecycle_id = Some(app.lifecycle_id.clone());
    match store.admit(&prod_again).await {
        Err(Error::OperationInProgress(blocker)) => {
            assert_eq!(blocker.scope, shared_types::UserAppOperationScope::Prod);
            assert_eq!(blocker.operation_id, prod_op.operation_id);
            assert_eq!(blocker.kind, Kind::Start);
            assert_eq!(blocker.state, State::Pending);
            assert_eq!(blocker.step, "admitted");
        }
        other => panic!("expected structured conflict, got {other:?}"),
    }

    // dev 先终态：prod 槽不受影响
    complete(&store, &dev_op, State::Succeeded).await;
    let after_dev = store.get_application(&app.app_id).await.unwrap().unwrap();
    assert_eq!(after_dev.active_operations.dev, None);
    assert_eq!(
        after_dev.active_operations.prod.as_deref(),
        Some(prod_op.operation_id.as_str())
    );

    // prod 终态后全槽清空
    complete(&store, &prod_op, State::Succeeded).await;
    let cleared = store.get_application(&app.app_id).await.unwrap().unwrap();
    assert!(cleared.active_operations.is_empty());
    assert_eq!(
        cleared.state,
        UserAppLifecycleState::Active,
        "Deleting only flips in the whole operation's own admission"
    );

    // 整体操作可受理：占 application 槽，Deleting 同事务生效
    let mut whole = request("terminal-delete", Kind::DeleteApplication);
    whole.app_id = app.app_id.clone();
    let whole_op = operation(store.admit(&whole).await.unwrap());
    let deleting = store.get_application(&app.app_id).await.unwrap().unwrap();
    assert_eq!(deleting.state, UserAppLifecycleState::Deleting);
    assert_eq!(
        deleting.active_operations.application.as_deref(),
        Some(whole_op.operation_id.as_str())
    );
}

#[tokio::test]
async fn closed_database_returns_error_not_absence_or_admission() {
    let (_directory, store) = database().await;
    store.shutdown().await.expect("shutdown");
    assert!(matches!(
        store.get_application("absent").await,
        Err(Error::Storage(_))
    ));
    assert!(matches!(
        store.admit(&request("new", Kind::EnsureBuilder)).await,
        Err(Error::Storage(_))
    ));
}

#[tokio::test]
async fn rejected_admission_does_not_leave_new_identity() {
    let (_directory, store) = database().await;
    let mut invalid = request("new", Kind::EnsureBuilder);
    invalid.request_fingerprint.clear();
    assert!(matches!(
        store.admit(&invalid).await,
        Err(Error::InvalidOperation(_))
    ));
    assert!(
        store
            .get_application("contract-app")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn joined_request_identity_remains_idempotent_after_completion() {
    let (_directory, store) = database().await;
    let op = operation(
        store
            .admit(&request("first", Kind::EnsureBuilder))
            .await
            .unwrap(),
    );
    let joined = request("second", Kind::EnsureBuilder);
    assert!(matches!(
        store.admit(&joined).await.unwrap(),
        Outcome::Existing(_)
    ));
    complete(&store, &op, State::Succeeded).await;
    let replay = store.admit(&joined).await.unwrap();
    assert!(
        matches!(replay, Outcome::Existing(_)),
        "joined request must not create again after completion"
    );
    assert_eq!(operation(replay).operation_id, op.operation_id);
    assert_eq!(
        store
            .get_operation_by_request("contract-app", "second")
            .await
            .unwrap()
            .unwrap()
            .operation_id,
        op.operation_id
    );
    assert!(
        store
            .get_operation_by_request("another-app", "second")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn metadata_cas_preserves_unmentioned_fields_and_noop_revision() {
    let (_directory, store) = database().await;
    let app = store.ensure_identity("contract-app").await.unwrap();
    let patch = shared_types::UserAppMetadataPatch {
        app_id: app.app_id,
        lifecycle_id: app.lifecycle_id,
        expected_revision: 1,
        name: Some(Some("A".into())),
        tenant_id: Some(Some("tenant".into())),
        space_id: None,
    };
    let changed = store.patch_metadata(&patch).await.unwrap();
    assert_eq!(changed.metadata_revision, 2);
    assert!(matches!(
        store.patch_metadata(&patch).await,
        Err(Error::VersionConflict)
    ));
    let noop = shared_types::UserAppMetadataPatch {
        expected_revision: 2,
        ..patch.clone()
    };
    assert_eq!(store.patch_metadata(&noop).await.unwrap(), changed);
    let rename = shared_types::UserAppMetadataPatch {
        name: Some(Some("B".into())),
        tenant_id: None,
        ..noop
    };
    let renamed = store.patch_metadata(&rename).await.unwrap();
    assert_eq!(renamed.tenant_id.as_deref(), Some("tenant"));
    assert_eq!(renamed.created_at, changed.created_at);
}

/// Run explicitly with --ignored and an isolated PostgreSQL DSN. Missing environment
/// fails this test; it never turns an explicit integration run into a skip.
#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires RCODER_USERAPP_PG_TEST_DSN for an isolated PostgreSQL instance"]
async fn postgres_real_transactions_and_restart_contract() {
    let dsn =
        std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("explicit PG integration DSN required");
    let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
    let schema = format!("userapp_test_{}", uuid::Uuid::new_v4().simple());
    toasty::sql::statement(format!("CREATE SCHEMA {schema}"))
        .exec(&mut admin)
        .await
        .unwrap();
    let separator = if dsn.contains('?') { '&' } else { '?' };
    let config = crate::config::PostgresConfig {
        url: Some(format!("{dsn}{separator}options=-csearch_path%3D{schema}")),
        ..Default::default()
    };
    let store = std::sync::Arc::new(PgUserAppStore::connect(&config).await.unwrap());
    let tested = store.clone();
    let result = tokio::spawn(async move {
        let store = tested.as_ref();
        storage_deletion_preserves_lifecycle(store).await;
        admission_metadata_is_atomic(store).await;
        request_identity_spans_recreation_and_control(store).await;
        paginated_scan_and_identity_contract(store).await;
        control_command_is_durable_and_part_of_deduplication(store).await;
        private_execution_input_contract(store).await;
        operation_lease_contract(store).await;
        operation_deadline_contract(store).await;
        physical_binding_is_atomic_and_cannot_cross_lifecycles(store).await;
        runtime_policy_commits_only_with_success(store).await;
        configuration_policy_is_transactional(store).await;
        deletion_success_requires_committed_evidence(store).await;
        control_snapshot_links_identity_and_operation(store).await;
        cross_scope_admission_is_independent(store).await;
        dev_uncertainty_does_not_block_prod(store).await;
        let a = store.ensure_identity("contract-app").await.unwrap();
        assert_eq!(a, store.ensure_identity("contract-app").await.unwrap());
        let first = request("first", Kind::EnsureBuilder);
        let second = request("second", Kind::EnsureBuilder);
        let (a, b) = tokio::join!(store.admit(&first), store.admit(&second));
        let op = operation(a.unwrap());
        assert_eq!(operation(b.unwrap()).operation_id, op.operation_id);
        let change = progress(&op, State::Running);
        let (a, b) = tokio::join!(store.advance(&change), store.advance(&change));
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        let running = store
            .get_operation("contract-app", &op.operation_id)
            .await
            .unwrap()
            .unwrap();
        store
            .advance(&progress(&running, State::Succeeded))
            .await
            .unwrap();
        // Both aliases must still point at the completed operation.
        assert_eq!(
            operation(store.admit(&first).await.unwrap()).operation_id,
            op.operation_id
        );
        assert_eq!(
            operation(store.admit(&second).await.unwrap()).operation_id,
            op.operation_id
        );
        let deletion = operation(
            store
                .admit(&request("delete", Kind::DeleteApplication))
                .await
                .unwrap(),
        );
        complete(store, &deletion, State::Succeeded).await;
        let next = store
            .recreate("contract-app", &op.lifecycle_id, "recreation")
            .await
            .unwrap();
        assert_eq!(next.lifecycle_epoch, 2);
        assert!(matches!(
            store.admit(&request("late", Kind::Start)).await,
            Err(Error::LifecycleConflict)
        ));
        assert!(matches!(
            store.advance(&change).await,
            Err(Error::LifecycleConflict)
        ));
        let invalid = UserAppAdmission {
            runtime_policy_on_success: None,
            command: None,
            metadata: None,
            app_id: "rolled-back".into(),
            request_fingerprint: String::new(),
            ..request("invalid", Kind::Start)
        };
        assert!(store.admit(&invalid).await.is_err());
        assert!(
            store
                .get_application("rolled-back")
                .await
                .unwrap()
                .is_none()
        );
        store.shutdown().await.unwrap();
        let restored = PgUserAppStore::connect(&config).await.unwrap();
        assert_eq!(
            restored
                .get_application("contract-app")
                .await
                .unwrap()
                .unwrap(),
            next
        );
        restored.shutdown().await.unwrap();
    })
    .await;
    store.shutdown().await.unwrap();
    // Only the generated schema owned by this test is removed, including when
    // its contract task panics. Production initialization never resets a schema.
    toasty::sql::statement(format!("DROP SCHEMA {schema} CASCADE"))
        .exec(&mut admin)
        .await
        .unwrap();
    result.expect("PG lifecycle contract failed");
}

async fn control_snapshot_links_identity_and_operation(store: &dyn UserAppLifecycleStore) {
    for app_id in ["snapshot-contract-a", "snapshot-contract-b"] {
        store.ensure_identity(app_id).await.expect("identity");
    }
    assert!(store.list_control_snapshots(None, 0).await.is_err());
    let before = store
        .list_control_snapshots(Some("snapshot-contract-"), 1)
        .await
        .expect("first page");
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].application.app_id, "snapshot-contract-a");
    assert!(before[0].operations.iter().next().is_none());
    let second = store
        .list_control_snapshots(Some(&before[0].application.app_id), 1)
        .await
        .expect("second page");
    assert_eq!(second[0].application.app_id, "snapshot-contract-b");
    let mut input = request("snapshot-control", Kind::Start);
    input.app_id = "snapshot-contract-a".into();
    let pending = operation(store.admit(&input).await.expect("admit"));
    let snapshot = store
        .list_control_snapshots(Some("snapshot-contract-"), 1)
        .await
        .expect("pending snapshot")
        .remove(0);
    assert_eq!(
        snapshot
            .application
            .active_operations
            .slot(pending.scope)
            .map(String::as_str),
        Some(pending.operation_id.as_str())
    );
    assert_eq!(
        snapshot.operations.slot(pending.scope),
        Some(&pending.clone())
    );
    assert!(snapshot.operations.iter().count() == 1);
    complete(store, &pending, State::Succeeded).await;
    let snapshot = store
        .list_control_snapshots(Some("snapshot-contract-"), 1)
        .await
        .expect("committed snapshot")
        .remove(0);
    assert!(snapshot.application.active_operations.is_empty());
    assert!(
        snapshot.operations.iter().next().is_none(),
        "historical completed operation is not an active operation"
    );
}

#[tokio::test]
async fn turso_control_snapshot_contract() {
    let (_directory, store) = database().await;
    control_snapshot_links_identity_and_operation(&store).await;
}

// 损坏数据反例（lifecycle 槽位指向不存在的操作）已由 Turso 后端原位
// 覆盖（`common::local_tests::broken_slot_is_rejected_instead_of_reported_idle`）。

async fn deletion_success_requires_committed_evidence(store: &dyn UserAppLifecycleStore) {
    use shared_types::{UserAppDeletionCheckpoint, UserAppDeletionStage as Stage};
    let mut input = request("guard-deletion", Kind::DeleteApplication);
    input.app_id = "deletion-stage-guard".into();
    let admitted = operation(store.admit(&input).await.expect("admit"));
    let mut claim = progress(&admitted, State::Running);
    claim.step = "claimed".into();
    claim.checkpoint = serde_json::Value::Null;
    let running = store.advance(&claim).await.expect("claim");
    let mut premature = progress(&running, State::Succeeded);
    premature.checkpoint = serde_json::Value::Null;
    assert!(matches!(
        store.advance(&premature).await,
        Err(Error::InvalidOperation(_))
    ));
    assert_eq!(
        store
            .get_operation(&input.app_id, &running.operation_id)
            .await
            .unwrap()
            .unwrap(),
        running
    );
    let mut checkpoint = UserAppDeletionCheckpoint {
        schema_version: 1,
        stage: Stage::Captured,
        kind: Kind::DeleteApplication,
        context: shared_types::UserAppExecutionContext {
            app_id: input.app_id.clone(),
            lifecycle_id: running.lifecycle_id.clone(),
            operation_id: running.operation_id.clone(),
            executor_id: "worker-A".into(),
            request_fingerprint: input.request_fingerprint.clone(),
        },
        production: shared_types::AppDeletionSnapshot {
            app_id: input.app_id.clone(),
            operation_id: "original-production".into(),
            resources: vec![],
        },
        development: Some(shared_types::UserappDevDeletionReceipt {
            runtime: shared_types::BuilderDeletionSnapshot {
                resource_binding: None,
                app_id: input.app_id.clone(),
                operation_id: "original-development".into(),
                resources: vec![],
                docker_bind_cleanup: false,
            },
            registry: None,
        }),
    };
    let mut update = progress(&running, State::Running);
    update.checkpoint = serde_json::to_value(&checkpoint).unwrap();
    let mut current = store.advance(&update).await.expect("capture");
    checkpoint.stage = Stage::DevelopmentRemoved;
    update = progress(&current, State::Running);
    update.checkpoint = serde_json::to_value(&checkpoint).unwrap();
    assert!(
        matches!(
            store.advance(&update).await,
            Err(Error::InvalidOperation(_))
        ),
        "no skipping confirmed boundaries"
    );
    checkpoint.stage = Stage::ComputeRemoved;
    checkpoint.production.operation_id = "replacement-targets".into();
    update.checkpoint = serde_json::to_value(&checkpoint).unwrap();
    assert!(
        matches!(
            store.advance(&update).await,
            Err(Error::InvalidOperation(_))
        ),
        "no replacing captured evidence"
    );
    let mut erased_failure = progress(&current, State::RecoveryRequired);
    erased_failure.checkpoint = serde_json::Value::Null;
    assert!(
        matches!(
            store.advance(&erased_failure).await,
            Err(Error::InvalidOperation(_))
        ),
        "failure cannot erase evidence"
    );
    checkpoint.production.operation_id = "original-production".into();
    for stage in [
        Stage::ComputeRemoved,
        Stage::ProductionStorageRemoved,
        Stage::DevelopmentRemoved,
    ] {
        checkpoint.stage = stage;
        update = progress(&current, State::Running);
        update.checkpoint = serde_json::to_value(&checkpoint).unwrap();
        current = store.advance(&update).await.expect("ordered stage");
    }
    let mut success = progress(&current, State::Succeeded);
    success.checkpoint = current.checkpoint.clone();
    store.advance(&success).await.expect("confirmed terminal");
    assert_eq!(
        store
            .get_application(&input.app_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        UserAppLifecycleState::Deleted
    );
}

#[tokio::test]
async fn turso_deletion_success_requires_evidence() {
    let (_directory, store) = database().await;
    deletion_success_requires_committed_evidence(&store).await;
}

async fn complete(store: &dyn UserAppLifecycleStore, op: &UserAppOperationRecord, state: State) {
    let deletion = matches!(
        op.kind,
        Kind::DeleteApplication | Kind::DeleteCompute | Kind::PurgeResources
    );
    let storage = matches!(op.kind, Kind::DestroyDevStorage | Kind::DestroyProdStorage);
    let mut claim = progress(op, State::Running);
    if deletion || storage {
        claim.step = "claimed".into();
        claim.checkpoint = serde_json::Value::Null;
    }
    let mut running = store.advance(&claim).await.unwrap();
    if storage && state == State::Succeeded {
        let _identity = store.get_application(&op.app_id).await.unwrap().unwrap();
        let evidence = shared_types::UserAppStorageDestruction {
            context: shared_types::UserAppExecutionContext {
                app_id: op.app_id.clone(),
                lifecycle_id: op.lifecycle_id.clone(),
                operation_id: op.operation_id.clone(),
                executor_id: "worker-A".into(),
                request_fingerprint: op.request_fingerprint.clone(),
            },
            production: (op.kind == Kind::DestroyProdStorage).then(|| {
                shared_types::AppDeletionSnapshot {
                    app_id: op.app_id.clone(),
                    operation_id: "storage-production".into(),
                    resources: vec![],
                }
            }),
            development: shared_types::UserappDevDeletionReceipt {
                runtime: shared_types::BuilderDeletionSnapshot {
                    resource_binding: None,
                    app_id: op.app_id.clone(),
                    operation_id: "storage-development".into(),
                    resources: vec![],
                    docker_bind_cleanup: false,
                },
                registry: None,
            },
        };
        let checkpoint = serde_json::to_value(evidence).unwrap();
        let steps: &[&str] = if op.kind == Kind::DestroyProdStorage {
            &[
                "storage_captured",
                "production_storage_removed",
                "development_storage_removed",
            ]
        } else {
            &["storage_captured", "development_storage_removed"]
        };
        for step in steps {
            let mut premature = progress(&running, State::Succeeded);
            premature.checkpoint = checkpoint.clone();
            assert!(
                store.advance(&premature).await.is_err(),
                "cannot succeed before final storage stage"
            );
            let mut update = progress(&running, State::Running);
            update.step = (*step).into();
            update.checkpoint = checkpoint.clone();
            running = store.advance(&update).await.unwrap();
        }
    }
    if deletion && state == State::Succeeded {
        use shared_types::{UserAppDeletionCheckpoint, UserAppDeletionStage as Stage};
        let _identity = store.get_application(&op.app_id).await.unwrap().unwrap();
        let mut checkpoint = UserAppDeletionCheckpoint {
            schema_version: 1,
            stage: Stage::Captured,
            kind: op.kind,
            context: shared_types::UserAppExecutionContext {
                app_id: op.app_id.clone(),
                lifecycle_id: op.lifecycle_id.clone(),
                operation_id: op.operation_id.clone(),
                executor_id: "worker-A".into(),
                request_fingerprint: op.request_fingerprint.clone(),
            },
            production: shared_types::AppDeletionSnapshot {
                app_id: op.app_id.clone(),
                operation_id: "fixture-production".into(),
                resources: vec![],
            },
            development: (op.kind != Kind::DeleteCompute).then(|| {
                shared_types::UserappDevDeletionReceipt {
                    runtime: shared_types::BuilderDeletionSnapshot {
                        resource_binding: None,
                        app_id: op.app_id.clone(),
                        operation_id: "fixture-development".into(),
                        resources: vec![],
                        docker_bind_cleanup: false,
                    },
                    registry: None,
                }
            }),
        };
        let stages: &[Stage] = if op.kind == Kind::DeleteCompute {
            &[Stage::Captured, Stage::ComputeRemoved]
        } else {
            &[
                Stage::Captured,
                Stage::ComputeRemoved,
                Stage::ProductionStorageRemoved,
                Stage::DevelopmentRemoved,
            ]
        };
        for stage in stages {
            checkpoint.stage = *stage;
            let mut update = progress(&running, State::Running);
            update.checkpoint = serde_json::to_value(&checkpoint).unwrap();
            running = store.advance(&update).await.unwrap();
        }
    }
    let mut terminal = progress(&running, state);
    if deletion || storage {
        terminal.checkpoint = running.checkpoint.clone();
    }
    store.advance(&terminal).await.unwrap();
}

#[tokio::test]
async fn another_executor_cannot_advance_a_running_operation() {
    let (_directory, store) = database().await;
    let op = operation(
        store
            .admit(&request("first", Kind::EnsureBuilder))
            .await
            .unwrap(),
    );
    let running = store.advance(&progress(&op, State::Running)).await.unwrap();
    let mut stolen = progress(&running, State::Succeeded);
    stolen.executor_id = "worker-B".into();
    assert!(matches!(
        store.advance(&stolen).await,
        Err(Error::InvalidOperation(_))
    ));
    assert_eq!(
        store
            .get_operation(&op.app_id, &op.operation_id)
            .await
            .unwrap()
            .unwrap(),
        running
    );
}

// 取消不留事务的等价保护已由 Turso worker 架构原位覆盖
// （`db::tests::cancelled_caller_does_not_cancel_admitted_transaction / abandoned_transaction_rolls_back_before_connection_reuse`）。

async fn private_execution_input_contract(store: &dyn UserAppLifecycleStore) {
    let input = shared_types::UserAppExecutionInput::new("{\"password\":\"private-token\"}".into());
    assert!(!format!("{input:?}").contains("private-token"));
    let mut req = request("private-input", Kind::Create);
    req.app_id = "private-input-app".into();
    req.command = Some(shared_types::UserAppControlCommand::Create {
        input_digest: input.digest(),
    });
    assert!(
        store.admit(&req).await.is_err(),
        "missing payload cannot be admitted"
    );
    assert!(
        store
            .get_operation(&req.app_id, &req.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    let wrong = shared_types::UserAppExecutionInput::new("different".into());
    assert!(store.admit_with_input(&req, Some(&wrong)).await.is_err());
    let op = operation(store.admit_with_input(&req, Some(&input)).await.unwrap());
    assert!(
        !serde_json::to_string(&op)
            .unwrap()
            .contains("private-token")
    );
    let context = shared_types::UserAppExecutionContext {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        executor_id: "worker-A".into(),
        request_fingerprint: op.request_fingerprint.clone(),
    };
    assert!(
        store.read_execution_input(&context).await.is_err(),
        "unclaimed input must be inaccessible"
    );
    let claimed = store.advance(&progress(&op, State::Running)).await.unwrap();
    assert_eq!(
        store
            .read_execution_input(&context)
            .await
            .unwrap()
            .encoded(),
        input.encoded()
    );
    let mut foreign = context.clone();
    foreign.executor_id = "other-worker".into();
    assert!(store.read_execution_input(&foreign).await.is_err());
    let mut changed = req.clone();
    changed.command = Some(shared_types::UserAppControlCommand::Create {
        input_digest: wrong.digest(),
    });
    assert!(
        store
            .admit_with_input(&changed, Some(&wrong))
            .await
            .is_err(),
        "same request cannot replace private input"
    );
    let completed = store
        .advance(&progress(&claimed, State::Succeeded))
        .await
        .unwrap();
    assert_eq!(completed.state, State::Succeeded);
    assert!(
        store.read_execution_input(&context).await.is_err(),
        "terminal input is no longer executable"
    );
    assert!(matches!(
        store.admit_with_input(&req, Some(&input)).await.unwrap(),
        Outcome::Existing(_)
    ));
}

#[tokio::test]
async fn turso_private_execution_input_contract() {
    let (_directory, store) = database().await;
    private_execution_input_contract(&store).await;
}

async fn physical_binding_is_atomic_and_cannot_cross_lifecycles(store: &dyn UserAppLifecycleStore) {
    let app = store
        .ensure_identity("binding-app")
        .await
        .expect("identity");
    let input = shared_types::UserAppExecutionInput::new(
        serde_json::to_string(&shared_types::AdoptBuilderRequest {
            lifecycle_id: app.lifecycle_id.clone(),
            request_id: "binding-adopt".into(),
            expected_container_id: "physical-original".into(),
        })
        .expect("input"),
    );
    let mut admission = request("binding-adopt", Kind::AdoptBuilder);
    admission.app_id = app.app_id.clone();
    admission.lifecycle_id = Some(app.lifecycle_id.clone());
    admission.request_fingerprint = input.digest();
    let accepted = operation(
        store
            .admit_with_input(&admission, Some(&input))
            .await
            .expect("admit"),
    );
    let running = store
        .advance(&UserAppOperationProgress {
            checkpoint: serde_json::Value::Null,
            ..progress(&accepted, State::Running)
        })
        .await
        .expect("claim");
    let binding = shared_types::UserAppResourceBinding {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        service_type: shared_types::ServiceType::UserappBuilder,
        physical_uid: "physical-original".into(),
        adopted_by_operation: running.operation_id.clone(),
    };
    let mut commit = progress(&running, State::Succeeded);
    commit.expected_revision += 1;
    assert!(
        store
            .commit_resource_binding(&binding, &commit)
            .await
            .is_err()
    );
    assert!(
        store
            .get_resource_binding(&binding.service_type, &binding.physical_uid)
            .await
            .expect("read")
            .is_none()
    );
    assert_eq!(
        store
            .get_operation(&app.app_id, &running.operation_id)
            .await
            .expect("read")
            .expect("operation")
            .state,
        State::Running
    );
    commit.expected_revision = running.revision;
    let done = store
        .commit_resource_binding(&binding, &commit)
        .await
        .expect("commit");
    assert_eq!(done.state, State::Succeeded);
    assert_eq!(
        store
            .get_resource_binding(&binding.service_type, &binding.physical_uid)
            .await
            .expect("read"),
        Some(binding.clone())
    );
    assert!(
        store
            .get_application(&app.app_id)
            .await
            .expect("read")
            .expect("identity")
            .active_operations
            .is_empty()
    );

    let mut deletion = request("binding-delete", Kind::DeleteApplication);
    deletion.app_id = app.app_id.clone();
    deletion.lifecycle_id = Some(app.lifecycle_id.clone());
    let deletion = operation(store.admit(&deletion).await.expect("delete admission"));
    complete(store, &deletion, State::Succeeded).await;
    let next = store
        .recreate(&app.app_id, &app.lifecycle_id, "binding-recreate")
        .await
        .expect("recreate");
    let input = shared_types::UserAppExecutionInput::new(
        serde_json::to_string(&shared_types::AdoptBuilderRequest {
            lifecycle_id: next.lifecycle_id.clone(),
            request_id: "replacement-adopt".into(),
            expected_container_id: binding.physical_uid.clone(),
        })
        .expect("input"),
    );
    let mut admission = request("replacement-adopt", Kind::AdoptBuilder);
    admission.app_id = app.app_id.clone();
    admission.lifecycle_id = Some(next.lifecycle_id.clone());
    admission.request_fingerprint = input.digest();
    let accepted = operation(
        store
            .admit_with_input(&admission, Some(&input))
            .await
            .expect("new admission"),
    );
    let running = store
        .advance(&UserAppOperationProgress {
            checkpoint: serde_json::Value::Null,
            ..progress(&accepted, State::Running)
        })
        .await
        .expect("claim");
    let replacement = shared_types::UserAppResourceBinding {
        lifecycle_id: next.lifecycle_id,
        adopted_by_operation: running.operation_id.clone(),
        ..binding.clone()
    };
    assert!(matches!(
        store
            .commit_resource_binding(&replacement, &progress(&running, State::Succeeded))
            .await,
        Err(Error::LifecycleConflict)
    ));
    assert_eq!(
        store
            .get_resource_binding(&binding.service_type, &binding.physical_uid)
            .await
            .expect("read"),
        Some(binding)
    );
    assert_eq!(
        store
            .get_operation(&app.app_id, &running.operation_id)
            .await
            .expect("read")
            .expect("operation")
            .state,
        State::Running
    );
}

#[tokio::test]
async fn turso_physical_binding_contract() {
    let (_directory, store) = database().await;
    physical_binding_is_atomic_and_cannot_cross_lifecycles(&store).await;
}

async fn operation_lease_contract(store: &dyn UserAppLifecycleStore) {
    for (suffix, kind, family, step) in [
        (
            "prod",
            Kind::Stop,
            shared_types::ServiceType::Userapp,
            "control_confirmed",
        ),
        (
            "builder",
            Kind::StopBuilder,
            shared_types::ServiceType::UserappBuilder,
            "compute_confirmed",
        ),
    ] {
        let mut req = request(&format!("lease-{suffix}"), kind);
        req.app_id = format!("lease-{suffix}");
        let pending = operation(store.admit(&req).await.unwrap());
        let running = store
            .advance(&progress(&pending, State::Running))
            .await
            .unwrap();
        let context = shared_types::UserAppExecutionContext {
            app_id: running.app_id.clone(),
            lifecycle_id: running.lifecycle_id.clone(),
            operation_id: running.operation_id.clone(),
            executor_id: "worker-A".into(),
            request_fingerprint: running.request_fingerprint.clone(),
        };
        let receipt = shared_types::UserAppOperationLeaseReceipt::Docker {
            service_type: family,
            device: 1,
            inode: 42,
            token: "lease-token".into(),
        };
        store
            .bind_operation_lease(&context, &receipt)
            .await
            .unwrap();
        store
            .bind_operation_lease(&context, &receipt)
            .await
            .unwrap();
        let binding = store
            .get_operation_lease(&req.app_id, &req.operation_id)
            .await
            .unwrap()
            .unwrap();
        let mut other = binding.clone();
        other.context.executor_id = "foreign-executor".into();
        assert!(
            store
                .bind_operation_lease(&other.context, &receipt)
                .await
                .is_err()
        );
        assert!(
            store.reserve_completed_operation(&running).await.is_err(),
            "unconfirmed write cannot authorize cleanup"
        );
        assert!(
            store.forget_operation_lease(&binding).await.is_err(),
            "running lease cannot be forgotten"
        );
        let mut checkpoint = progress(&running, State::Running);
        checkpoint.step = step.into();
        checkpoint.checkpoint = serde_json::json!({"target":{"context":context},"result":{"operation_id":running.operation_id}});
        let confirmed = store.advance(&checkpoint).await.unwrap();
        assert!(shared_types::userapp_operation_has_final_evidence(
            &confirmed
        ));
        let reserved = store.reserve_completed_operation(&confirmed).await.unwrap();
        assert!(
            store.reserve_completed_operation(&confirmed).await.is_err(),
            "stale completion cannot reserve twice"
        );
        let mut failed_release = progress(&reserved, State::RecoveryRequired);
        failed_release.step = reserved.step.clone();
        failed_release.checkpoint = reserved.checkpoint.clone();
        failed_release.error_message = Some("Conditional lease release failed".into());
        let recovery = store.advance(&failed_release).await.unwrap();
        assert!(shared_types::userapp_operation_has_final_evidence(
            &recovery
        ));
        assert_eq!(
            store
                .get_operation_lease(&req.app_id, &req.operation_id)
                .await
                .unwrap(),
            Some(binding.clone())
        );
        let retried = store.reserve_completed_operation(&recovery).await.unwrap();
        let mut success = progress(&retried, State::Succeeded);
        success.step = retried.step.clone();
        success.checkpoint = retried.checkpoint.clone();
        store.advance(&success).await.unwrap();
        assert!(
            store
                .terminal_operation_leases(None, 100)
                .await
                .unwrap()
                .contains(&binding)
        );
        assert!(store.forget_operation_lease(&other).await.is_err());
        store.forget_operation_lease(&binding).await.unwrap();
        store.forget_operation_lease(&binding).await.unwrap();
        assert!(
            store
                .get_operation_lease(&req.app_id, &req.operation_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn turso_operation_lease_contract() {
    let (_directory, store) = database().await;
    operation_lease_contract(&store).await;
}

/// Deadline bind-once side-record contract:
/// - identity checks (lifecycle mismatch → LifecycleConflict, terminal → VersionConflict)
/// - bind-once semantics (second bind returns persisted value, different deadline not accepted)
/// - read returns None for unknown app/operation
async fn operation_deadline_contract(store: &dyn UserAppLifecycleStore) {
    // --- fresh app, no deadline yet ---
    let mut req = request("dl-fresh", Kind::Update);
    req.app_id = "dl-app".into();
    let op = operation(store.admit(&req).await.unwrap());
    assert!(
        store
            .operation_deadline(&req.app_id, &op.operation_id)
            .await
            .unwrap()
            .is_none(),
        "no deadline before first bind"
    );

    // --- first bind succeeds, returns the value ---
    let deadline_a: i64 = 1_000_000;
    let returned = store
        .bind_operation_deadline(&req.app_id, &op.operation_id, &op.lifecycle_id, deadline_a)
        .await
        .unwrap();
    assert_eq!(returned, deadline_a);

    // --- bind-once: second bind with different deadline returns persisted value ---
    let deadline_b: i64 = 9_999_999;
    let returned = store
        .bind_operation_deadline(&req.app_id, &op.operation_id, &op.lifecycle_id, deadline_b)
        .await
        .unwrap();
    assert_eq!(returned, deadline_a, "bind-once must return first value");
    assert_eq!(
        store
            .operation_deadline(&req.app_id, &op.operation_id)
            .await
            .unwrap(),
        Some(deadline_a)
    );

    // --- lifecycle mismatch → LifecycleConflict ---
    assert!(matches!(
        store
            .bind_operation_deadline(&req.app_id, &op.operation_id, "wrong-lifecycle", deadline_a)
            .await,
        Err(Error::LifecycleConflict)
    ));

    // --- wrong app_id → LifecycleConflict (app lifecycle_id won't match) ---
    assert!(matches!(
        store
            .bind_operation_deadline("wrong-app", &op.operation_id, &op.lifecycle_id, deadline_a)
            .await,
        Err(Error::NotFound)
    ));

    // --- unknown operation → LifecycleConflict (identity check fires first) ---
    assert!(matches!(
        store
            .bind_operation_deadline(&req.app_id, "no-such-op", &op.lifecycle_id, deadline_a)
            .await,
        Err(Error::LifecycleConflict)
    ));

    // --- read unknown app → None ---
    assert!(
        store
            .operation_deadline("nonexistent", "nonexistent")
            .await
            .unwrap()
            .is_none(),
        "unknown app/operation returns None"
    );

    // --- terminal operation → LifecycleConflict (current_operation_id cleared) ---
    complete(store, &op, State::Succeeded).await;
    // After completion, current_operation_id is cleared; identity check fires first
    let dl_result = store
        .bind_operation_deadline(&req.app_id, &op.operation_id, &op.lifecycle_id, deadline_a)
        .await;
    assert!(
        matches!(dl_result, Err(Error::LifecycleConflict)),
        "completed operation no longer matches current_operation_id"
    );

    // --- read still returns the persisted deadline after terminal ---
    assert_eq!(
        store
            .operation_deadline(&req.app_id, &op.operation_id)
            .await
            .unwrap(),
        Some(deadline_a),
        "deadline side-record persists after operation completion"
    );
}

#[tokio::test]
async fn turso_operation_deadline_contract() {
    let (_directory, store) = database().await;
    operation_deadline_contract(&store).await;
}
