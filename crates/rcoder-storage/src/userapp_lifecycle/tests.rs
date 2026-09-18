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
async fn database() -> (tempfile::TempDir, SqliteUserAppStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteUserAppStore::open(&directory.path().join("userapp.sqlite3"))
        .await
        .unwrap();
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

#[tokio::test]
async fn sqlite_failure_after_operation_insert_rolls_back_entire_admission() {
    let (directory, store) = database().await;
    let app = store.ensure_identity("atomic-failure").await.unwrap();
    let observer = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(directory.path().join("userapp.sqlite3")),
        )
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_lifecycle_update BEFORE UPDATE ON userapp_lifecycles BEGIN SELECT RAISE(FAIL, 'injected lifecycle write failure'); END")
        .execute(&observer).await.unwrap();
    let mut req = request("atomic-failure-operation", Kind::Update);
    req.app_id = app.app_id.clone();
    req.lifecycle_id = Some(app.lifecycle_id.clone());
    req.metadata = Some(shared_types::UserAppMetadataPatch {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        expected_revision: app.metadata_revision,
        name: Some(Some("must-not-commit".into())),
        tenant_id: None,
        space_id: None,
    });
    assert!(matches!(store.admit(&req).await, Err(Error::Storage(_))));
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
    sqlx::query("DROP TRIGGER fail_lifecycle_update")
        .execute(&observer)
        .await
        .unwrap();
    assert!(matches!(
        store.admit(&req).await.unwrap(),
        Outcome::Accepted(_)
    ));
    observer.close().await;
}

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

async fn paginated_scan_and_import_contract(store: &dyn UserAppLifecycleStore) {
    let old = shared_types::AppMetadataRecord {
        app_id: "import-contract".into(),
        generation: "legacy-generation".into(),
        name: Some("original".into()),
        tenant_id: Some("tenant".into()),
        space_id: None,
        created_at: chrono::Utc::now() - chrono::Duration::days(1),
    };
    let imported = store.import_application(&old).await.unwrap();
    assert_eq!(imported.created_at, old.created_at);
    assert_eq!(imported.name, old.name);
    assert_eq!(imported, store.import_application(&old).await.unwrap());
    let mut delete = request("import-delete", Kind::DeleteApplication);
    delete.app_id = old.app_id.clone();
    let op = operation(store.admit(&delete).await.unwrap());
    complete(store, &op, State::Succeeded).await;
    let tombstone = store.import_application(&old).await.unwrap();
    assert_eq!(tombstone.state, UserAppLifecycleState::Deleted);
    assert_eq!(tombstone.lifecycle_id, imported.lifecycle_id);

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
    assert!(apps.contains(&old.app_id));
    assert!(apps.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        apps.iter().filter(|id| id.starts_with("scan-app-")).count(),
        5
    );
    assert!(store.list_applications(None, 0).await.is_err());
    assert!(store.unfinished_operations(None, 0).await.is_err());
}

#[tokio::test]
async fn sqlite_paginated_recovery_and_legacy_import() {
    let (_directory, store) = database().await;
    paginated_scan_and_import_contract(&store).await;
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
async fn sqlite_control_command_persistence_contract() {
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
async fn sqlite_configuration_policy_contract() {
    let (_directory, store) = database().await;
    configuration_policy_is_transactional(&store).await;
}

#[tokio::test]
async fn sqlite_policy_commit_contract() {
    let (_directory, store) = database().await;
    runtime_policy_commits_only_with_success(&store).await;
}

#[tokio::test]
async fn sqlite_instance_directory_is_exclusive_and_restart_preserves_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("userapp.sqlite3");
    let first = SqliteUserAppStore::open_exclusive(&path).await.unwrap();
    let before = first.ensure_identity("persistent-app").await.unwrap();
    assert!(SqliteUserAppStore::open_exclusive(&path).await.is_err());
    // A second database filename cannot hide sharing the same instance directory.
    assert!(
        SqliteUserAppStore::open_exclusive(&dir.path().join("other.sqlite3"))
            .await
            .is_err()
    );
    first.close().await;
    drop(first);
    let restarted = SqliteUserAppStore::open_exclusive(&path).await.unwrap();
    assert_eq!(
        before,
        restarted
            .get_application("persistent-app")
            .await
            .unwrap()
            .unwrap()
    );
    restarted.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn sqlite_directory_alias_uses_the_same_instance_lock() {
    let root = tempfile::tempdir().expect("SQLite test directory");
    let real = root.path().join("real");
    let alias = root.path().join("alias");
    std::fs::create_dir(&real).expect("real directory");
    std::os::unix::fs::symlink(&real, &alias).expect("directory alias");
    let first = SqliteUserAppStore::open_exclusive(&real.join("userapp.sqlite3"))
        .await
        .expect("first instance");
    let before = first
        .ensure_identity("directory-alias")
        .await
        .expect("identity");
    assert!(
        SqliteUserAppStore::open_exclusive(&alias.join("userapp.sqlite3"))
            .await
            .is_err()
    );
    first.close().await;
    drop(first);
    let reopened = SqliteUserAppStore::open_exclusive(&alias.join("userapp.sqlite3"))
        .await
        .expect("reopen through alias");
    assert_eq!(
        reopened
            .get_application("directory-alias")
            .await
            .expect("read")
            .expect("identity"),
        before
    );
    reopened.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn sqlite_database_file_alias_cannot_bypass_directory_ownership() {
    let root = tempfile::tempdir().expect("SQLite test directory");
    let real = root.path().join("real");
    std::fs::create_dir(&real).expect("database directory");
    let path = real.join("userapp.sqlite3");
    let first = SqliteUserAppStore::open_exclusive(&path)
        .await
        .expect("first instance");
    let identity = first.ensure_identity("file-alias").await.expect("identity");
    for hard_link in [false, true] {
        let alias_directory = root
            .path()
            .join(if hard_link { "hard" } else { "symbolic" });
        std::fs::create_dir(&alias_directory).expect("alias directory");
        let alias = alias_directory.join("userapp.sqlite3");
        if hard_link {
            std::fs::hard_link(&path, &alias).expect("hard link");
        } else {
            std::os::unix::fs::symlink(&path, &alias).expect("symbolic link");
        }
        assert!(SqliteUserAppStore::open_exclusive(&alias).await.is_err());
        assert!(!alias_directory.join("userapp.sqlite3-wal").exists());
        assert!(!alias_directory.join("userapp.sqlite3-shm").exists());
        std::fs::remove_file(&alias).expect("remove test alias");
    }
    assert_eq!(
        first
            .get_application("file-alias")
            .await
            .expect("read")
            .expect("identity"),
        identity
    );
    first.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn sqlite_linked_sidecars_and_lock_fail_before_touching_the_target() {
    for filename in [
        "userapp.sqlite3-wal",
        "userapp.sqlite3-shm",
        "userapp.sqlite3-journal",
        ".userapp-instance.lock",
    ] {
        for hard_link in [false, true] {
            let root = tempfile::tempdir().expect("SQLite test directory");
            let sentinel = root.path().join("sentinel");
            std::fs::write(&sentinel, b"must remain unchanged").expect("sentinel");
            let alias = root.path().join(filename);
            if hard_link {
                std::fs::hard_link(&sentinel, &alias).expect("hard link");
            } else {
                std::os::unix::fs::symlink(&sentinel, &alias).expect("symbolic link");
            }
            let database = root.path().join("userapp.sqlite3");
            assert!(
                SqliteUserAppStore::open_exclusive(&database).await.is_err(),
                "{filename}"
            );
            assert_eq!(
                std::fs::read(&sentinel).expect("sentinel bytes"),
                b"must remain unchanged"
            );
            assert!(!database.exists(), "validation precedes database creation");
        }
    }
}

#[tokio::test]
async fn sqlite_failed_initialization_releases_the_instance_lock() {
    let root = tempfile::tempdir().expect("SQLite test directory");
    let database = root.path().join("userapp.sqlite3");
    std::fs::write(&database, b"not a SQLite database").expect("invalid test database");
    assert!(SqliteUserAppStore::open_exclusive(&database).await.is_err());
    assert_eq!(
        std::fs::read(&database).expect("preserved bytes"),
        b"not a SQLite database"
    );
    // Repair only this test-owned fixture; a failed production startup never
    // deletes, truncates or silently replaces a database.
    std::fs::remove_file(&database).expect("remove invalid fixture");
    let repaired = SqliteUserAppStore::open_exclusive(&database)
        .await
        .expect("lock released after initialization error");
    repaired
        .ensure_identity("repaired")
        .await
        .expect("durable write");
    repaired.close().await;
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
    store.close().await;
    let restored = SqliteUserAppStore::open(&directory.path().join("userapp.sqlite3"))
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
async fn sqlite_cross_scope_admission_is_independent() {
    let (_directory, store) = database().await;
    cross_scope_admission_is_independent(&store).await;
}

#[tokio::test]
async fn sqlite_dev_uncertainty_does_not_block_prod() {
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

/// §7.8：旧单指针 JSON 经 0007 迁移进槽位；终态指向不占槽；悬空指针/未知
/// kind 阻断迁移且不半提交；重复打开幂等。
#[tokio::test]
async fn sqlite_legacy_pointer_records_migrate_into_scope_slots() {
    let (directory, store) = database().await;
    // 旧不变量：每个 app 至多一个在途操作。分别构造 dev/prod 在途、
    // 终态指向（防御性）与无操作四种旧形态。
    let mut dev = request("legacy-dev", Kind::RestartBuilder);
    dev.app_id = "legacy-dev-app".into();
    let dev_op = operation(store.admit(&dev).await.unwrap());
    let dev_running = store
        .advance(&progress(&dev_op, State::Running))
        .await
        .unwrap();
    let mut prod = request("legacy-prod", Kind::Start);
    prod.app_id = "legacy-prod-app".into();
    let prod_op = operation(store.admit(&prod).await.unwrap());
    complete(&store, &prod_op, State::RecoveryRequired).await;
    let mut terminal = request("legacy-terminal", Kind::Stop);
    terminal.app_id = "legacy-terminal-app".into();
    let terminal_op = operation(store.admit(&terminal).await.unwrap());
    complete(&store, &terminal_op, State::Succeeded).await;
    store.ensure_identity("legacy-idle-app").await.unwrap();
    store.close().await;

    // 降级到旧 JSON 形态：去掉 scope、槽位还原为 current_operation_id 单指针
    let legacy = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(directory.path().join("userapp.sqlite3")),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE userapp_operations SET record=json_remove(record,'$.scope')")
        .execute(&legacy)
        .await
        .unwrap();
    for (app_id, pointer) in [
        ("legacy-dev-app", Some(dev_running.operation_id.clone())),
        ("legacy-prod-app", Some(prod_op.operation_id.clone())),
        // 防御性旧数据：终态操作仍被指针引用（终态不得迁移为占槽）
        (
            "legacy-terminal-app",
            Some(terminal_op.operation_id.clone()),
        ),
        ("legacy-idle-app", None),
    ] {
        sqlx::query("UPDATE userapp_lifecycles SET record=json_set(json_remove(record,'$.active_operations'),'$.current_operation_id',$2) WHERE app_id=$1")
            .bind(app_id)
            .bind(pointer)
            .execute(&legacy)
            .await
            .unwrap();
    }
    sqlx::query("DELETE FROM _sqlx_userapp_migrations WHERE version=7")
        .execute(&legacy)
        .await
        .unwrap();
    legacy.close().await;

    let migrated = SqliteUserAppStore::open(&directory.path().join("userapp.sqlite3"))
        .await
        .expect("legacy database must migrate");
    let dev_app = migrated
        .get_application("legacy-dev-app")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        dev_app.active_operations.dev.as_deref(),
        Some(dev_running.operation_id.as_str())
    );
    assert_eq!(dev_app.active_operations.prod, None);
    let prod_app = migrated
        .get_application("legacy-prod-app")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        prod_app.active_operations.prod.as_deref(),
        Some(prod_op.operation_id.as_str())
    );
    assert_eq!(prod_app.active_operations.dev, None);
    let terminal_app = migrated
        .get_application("legacy-terminal-app")
        .await
        .unwrap()
        .unwrap();
    assert!(
        terminal_app.active_operations.is_empty(),
        "terminal pointed operation must not occupy a slot"
    );
    let idle_app = migrated
        .get_application("legacy-idle-app")
        .await
        .unwrap()
        .unwrap();
    assert!(idle_app.active_operations.is_empty());
    for (app_id, scope, expected) in [
        (
            "legacy-dev-app",
            shared_types::UserAppOperationScope::Dev,
            dev_running.operation_id.clone(),
        ),
        (
            "legacy-prod-app",
            shared_types::UserAppOperationScope::Prod,
            prod_op.operation_id.clone(),
        ),
    ] {
        let record = migrated
            .get_operation(app_id, &expected)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.scope, scope);
    }
    migrated.close().await;

    // 幂等：迁移记录后再次打开不重复改写
    let again = SqliteUserAppStore::open(&directory.path().join("userapp.sqlite3"))
        .await
        .expect("idempotent reopen");
    assert_eq!(
        again
            .get_application("legacy-prod-app")
            .await
            .unwrap()
            .unwrap(),
        prod_app
    );
    again.close().await;
}

/// §7.8 反例：悬空指针与未知 kind 必须阻断迁移（事务回滚，不留半提交）。
#[tokio::test]
async fn sqlite_scope_migration_aborts_on_dangling_pointer_and_unknown_kind() {
    for label in ["dangling", "unknown-kind"] {
        let (directory, store) = database().await;
        let mut req = request(&format!("legacy-{label}"), Kind::Start);
        req.app_id = format!("legacy-{label}-app");
        let op = operation(store.admit(&req).await.unwrap());
        let running = store.advance(&progress(&op, State::Running)).await.unwrap();
        store.close().await;
        let legacy = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(directory.path().join("userapp.sqlite3")),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE userapp_operations SET record=json_remove(record,'$.scope')")
            .execute(&legacy)
            .await
            .unwrap();
        match label {
            "dangling" => {
                sqlx::query("UPDATE userapp_lifecycles SET record=json_set(json_remove(record,'$.active_operations'),'$.current_operation_id','ghost-operation') WHERE app_id=$1")
                    .bind(&req.app_id)
                    .execute(&legacy)
                    .await
                    .unwrap();
            }
            _ => {
                sqlx::query("UPDATE userapp_operations SET record=json_set(record,'$.kind','Nonsense') WHERE operation_id=$1")
                    .bind(&running.operation_id)
                    .execute(&legacy)
                    .await
                    .unwrap();
                sqlx::query("UPDATE userapp_lifecycles SET record=json_set(json_remove(record,'$.active_operations'),'$.current_operation_id',$2) WHERE app_id=$1")
                    .bind(&req.app_id)
                    .bind(&running.operation_id)
                    .execute(&legacy)
                    .await
                    .unwrap();
            }
        }
        sqlx::query("DELETE FROM _sqlx_userapp_migrations WHERE version=7")
            .execute(&legacy)
            .await
            .unwrap();
        legacy.close().await;
        let result = SqliteUserAppStore::open(&directory.path().join("userapp.sqlite3")).await;
        assert!(result.is_err(), "{label} must block the migration");
        // 失败不半提交：直连检查旧形态未被改写（scope 仍缺失）
        let inspect = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(directory.path().join("userapp.sqlite3")),
            )
            .await
            .unwrap();
        let raw: String =
            sqlx::query_scalar("SELECT record FROM userapp_operations WHERE operation_id=$1")
                .bind(&running.operation_id)
                .fetch_one(&inspect)
                .await
                .unwrap();
        assert!(!raw.contains("\"scope\""), "{label} rolled back fully");
        inspect.close().await;
    }
}

#[tokio::test]
async fn closed_database_returns_error_not_absence_or_admission() {
    let (_directory, store) = database().await;
    store.close().await;
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

/// Run explicitly with --ignored and an isolated PG 17 DSN. Missing environment
/// fails this test; it never turns an explicit integration run into a skip.
#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires RCODER_USERAPP_PG_TEST_DSN for an isolated PG 17 instance"]
async fn postgres_real_transactions_and_restart_contract() {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::str::FromStr;
    let dsn =
        std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("explicit PG integration DSN required");
    let admin = sqlx::PgPool::connect(&dsn).await.unwrap();
    let schema = format!("userapp_test_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let options = PgConnectOptions::from_str(&dsn)
        .unwrap()
        .options([("search_path", schema.as_str())]);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options.clone())
        .await
        .unwrap();
    let store = PgUserAppStore::open(pool.clone()).await.unwrap();
    let result = tokio::spawn(async move {
        use shared_types::AppMetadataPersistence as _;
        // Exercise the actual legacy schema/migrations and importer, not just a
        // newly constructed record passed directly to the new storage contract.
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        let legacy = crate::pg::userapp::metadata::PgAppMetadataPersistence::new(pool.clone());
        let old = shared_types::AppMetadataRecord {
            app_id: "legacy-pg-app".into(),
            generation: "legacy-pg-generation".into(),
            name: Some("original name".into()),
            tenant_id: None,
            space_id: None,
            created_at: chrono::Utc::now() - chrono::Duration::days(2),
        };
        legacy.upsert(&old).await.unwrap();
        store.import_legacy_metadata().await.unwrap();
        let imported = store.get_application(&old.app_id).await.unwrap().unwrap();
        assert_eq!(imported.name, old.name);
        assert_eq!(
            imported.created_at.timestamp_micros(),
            old.created_at.timestamp_micros()
        );
        let mut deletion = request("legacy-pg-delete", Kind::DeleteApplication);
        deletion.app_id = old.app_id.clone();
        complete(
            &store,
            &operation(store.admit(&deletion).await.unwrap()),
            State::Succeeded,
        )
        .await;
        store.import_legacy_metadata().await.unwrap();
        assert_eq!(
            store
                .get_application(&old.app_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            UserAppLifecycleState::Deleted
        );
        storage_deletion_preserves_lifecycle(&store).await;
        admission_metadata_is_atomic(&store).await;
        request_identity_spans_recreation_and_control(&store).await;
        paginated_scan_and_import_contract(&store).await;
        control_command_is_durable_and_part_of_deduplication(&store).await;
        private_execution_input_contract(&store).await;
        operation_lease_contract(&store).await;
        operation_deadline_contract(&store).await;
        physical_binding_is_atomic_and_cannot_cross_lifecycles(&store).await;
        runtime_policy_commits_only_with_success(&store).await;
        configuration_policy_is_transactional(&store).await;
        deletion_success_requires_committed_evidence(&store).await;
        control_snapshot_links_identity_and_operation(&store).await;
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
        complete(&store, &deletion, State::Succeeded).await;
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
        pool.close().await;
        let reopened = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        let restored = PgUserAppStore::open(reopened.clone()).await.unwrap();
        assert_eq!(
            restored
                .get_application("contract-app")
                .await
                .unwrap()
                .unwrap(),
            next
        );
        reopened.close().await;
    })
    .await;
    // The schema is generated exclusively by this test; cleanup also runs if an
    // assertion in the isolated worker panics.
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
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
async fn sqlite_control_snapshot_contract() {
    let (_directory, store) = database().await;
    control_snapshot_links_identity_and_operation(&store).await;
}

#[tokio::test]
async fn sqlite_control_snapshot_rejects_broken_operation_link() {
    let (directory, store) = database().await;
    let mut identity = store
        .ensure_identity("snapshot-corrupt")
        .await
        .expect("identity");
    identity.active_operations.prod = Some("missing-operation".into());
    let connection = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(directory.path().join("userapp.sqlite3")),
        )
        .await
        .expect("fault injection connection");
    sqlx::query("UPDATE userapp_lifecycles SET record=$1 WHERE app_id=$2")
        .bind(serde_json::to_string(&identity).expect("record"))
        .bind("snapshot-corrupt")
        .execute(&connection)
        .await
        .expect("inject broken link");
    assert!(
        matches!(
            store.list_control_snapshots(None, 128).await,
            Err(Error::InvalidOperation(_))
        ),
        "missing operation cannot appear as normal idle state"
    );
    connection.close().await;
}

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
async fn sqlite_deletion_success_requires_evidence() {
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

#[tokio::test]
async fn cancelled_admission_waiting_for_sqlite_writer_does_not_leak_a_transaction() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (directory, store) = database().await;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(directory.path().join("userapp.sqlite3"));
        let competitor = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        let held = competitor.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let request = request("cancelled", Kind::EnsureBuilder);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), store.admit(&request))
                .await
                .is_err()
        );
        held.rollback().await.unwrap();
        assert!(
            store
                .get_application("contract-app")
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            store.admit(&request).await.unwrap(),
            Outcome::Accepted(_)
        ));
        competitor.close().await;
        store.close().await;
    })
    .await
    .expect("cancelled SQLite transaction must release its connection");
}

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
async fn sqlite_private_execution_input_contract() {
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
async fn sqlite_physical_binding_contract() {
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
async fn sqlite_operation_lease_contract() {
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
async fn sqlite_operation_deadline_contract() {
    let (_directory, store) = database().await;
    operation_deadline_contract(&store).await;
}
