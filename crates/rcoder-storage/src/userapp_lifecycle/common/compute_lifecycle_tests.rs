//! Cross-lifecycle contracts use the real ledger and deletion checkpoints.
use super::{ToastyUserAppStore, compute_identity, compute_progress, request};
use crate::db::models;
use shared_types::*;

pub(super) fn accepted(value: UserAppAdmissionOutcome) -> UserAppOperationRecord {
    match value {
        UserAppAdmissionOutcome::Accepted(op) => op,
        UserAppAdmissionOutcome::Existing(op) => panic!("fresh operation required: {op:?}"),
    }
}

pub(super) fn business_request(
    app: &UserAppLifecycleRecord,
    kind: UserAppOperationKind,
) -> UserAppAdmission {
    UserAppAdmission {
        app_id: app.app_id.clone(),
        lifecycle_id: Some(app.lifecycle_id.clone()),
        operation_id: uuid::Uuid::new_v4().simple().to_string(),
        request_id: Some(uuid::Uuid::new_v4().simple().to_string()),
        request_fingerprint: "b".repeat(64),
        kind,
        command: None,
        metadata: None,
        runtime_policy_on_success: None,
    }
}

fn business_progress(
    op: &UserAppOperationRecord,
    state: UserAppOperationState,
    checkpoint: serde_json::Value,
) -> UserAppOperationProgress {
    UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: "lifecycleworker".into(),
        state,
        step: "lifecycle_contract".into(),
        checkpoint,
        error_code: None,
        error_message: None,
    }
}

pub(super) async fn delete_application(
    store: &ToastyUserAppStore,
    app: &UserAppLifecycleRecord,
) -> UserAppOperationRecord {
    let mut op = accepted(
        store
            .admit(&business_request(
                app,
                UserAppOperationKind::DeleteApplication,
            ))
            .await
            .unwrap(),
    );
    op = store
        .advance(&business_progress(
            &op,
            UserAppOperationState::Running,
            serde_json::Value::Null,
        ))
        .await
        .unwrap();
    let mut checkpoint = UserAppDeletionCheckpoint {
        schema_version: 1,
        stage: UserAppDeletionStage::Captured,
        kind: UserAppOperationKind::DeleteApplication,
        context: UserAppExecutionContext {
            app_id: op.app_id.clone(),
            lifecycle_id: op.lifecycle_id.clone(),
            operation_id: op.operation_id.clone(),
            executor_id: "lifecycleworker".into(),
            request_fingerprint: op.request_fingerprint.clone(),
        },
        production: AppDeletionSnapshot {
            app_id: app.app_id.clone(),
            operation_id: "productionfixture".into(),
            resources: vec![],
            directories: None,
        },
        development: Some(UserappDevDeletionReceipt {
            runtime: BuilderDeletionSnapshot {
                resource_binding: None,
                app_id: app.app_id.clone(),
                operation_id: "developmentfixture".into(),
                resources: vec![],
                docker_bind_cleanup: false,
            },
            registry: None,
            lease: None,
            collaborators: vec![],
            directories: None,
        }),
    };
    for stage in [
        UserAppDeletionStage::Captured,
        UserAppDeletionStage::ComputeRemoved,
        UserAppDeletionStage::ProductionStorageRemoved,
        UserAppDeletionStage::DevelopmentRemoved,
    ] {
        checkpoint.stage = stage;
        op = store
            .advance(&business_progress(
                &op,
                UserAppOperationState::Running,
                serde_json::to_value(&checkpoint).unwrap(),
            ))
            .await
            .unwrap();
    }
    store
        .advance(&business_progress(
            &op,
            UserAppOperationState::Succeeded,
            op.checkpoint.clone(),
        ))
        .await
        .unwrap()
}

pub(super) async fn completed_compute(
    store: &ToastyUserAppStore,
    app: &UserAppLifecycleRecord,
    scope: UserAppOperationScope,
    action: ComputeControlAction,
    retain_receipt: bool,
) -> ComputeControlRecord {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let mut req = request(app, &id, action);
    req.scope = scope;
    let pending = store.admit_compute_control(&req).await.unwrap();
    let identity = compute_identity(&pending);
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let receipt = UserAppOperationLeaseReceipt::Docker {
        service_type: match scope {
            UserAppOperationScope::Dev => ServiceType::UserappBuilder,
            UserAppOperationScope::Prod => ServiceType::Userapp,
            UserAppOperationScope::Application => panic!("not a compute scope"),
        },
        device: 1,
        inode: 42,
        token: format!("fixture{id}"),
    };
    let mut record = store.bind_compute_lease(&identity, &receipt).await.unwrap();
    let stages: &[ComputeControlStage] = match action {
        ComputeControlAction::Stop => &[
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Completed,
        ],
        ComputeControlAction::Restart => &[
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Starting,
            ComputeControlStage::Verifying,
            ComputeControlStage::Completed,
        ],
    };
    for stage in stages {
        let mut progress = compute_progress(&record, *stage);
        progress.state = match stage {
            ComputeControlStage::Completed => ComputeControlState::Succeeded,
            ComputeControlStage::DrainingPrevious
            | ComputeControlStage::Stopping
            | ComputeControlStage::Stopped
            | ComputeControlStage::Starting
            | ComputeControlStage::Verifying => ComputeControlState::Running,
        };
        record = store.advance_compute_control(&progress).await.unwrap();
    }
    if !retain_receipt {
        store
            .forget_compute_lease(&identity, &receipt)
            .await
            .unwrap();
    }
    store
        .get_compute_control(&app.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap()
}

pub(super) async fn raw_control(
    store: &ToastyUserAppStore,
    operation_id: &str,
) -> serde_json::Value {
    let id = operation_id.to_owned();
    store
        .run(true, move |tx, _| {
            let id = id.clone();
            Box::pin(async move {
                let row = models::ComputeControl::get_by_operation_id(tx, &id)
                    .await
                    .map_err(super::super::storage)?;
                Ok(serde_json::json!({
                    "operation_id": row.operation_id, "app_id": row.app_id,
                    "lifecycle_id": row.lifecycle_id, "scope": row.scope,
                    "generation": row.generation, "revision": row.revision,
                    "action": row.action, "state": row.state,
                    "request_id": row.request_id, "request_fingerprint": row.request_fingerprint,
                    "executor_id": row.executor_id, "stage": row.stage,
                    "evidence_json": row.evidence_json, "checkpoint_json": row.checkpoint_json,
                    "lease_json": row.lease_json, "error_code": row.error_code,
                    "error_message": row.error_message, "created_at_us": row.created_at_us,
                    "updated_at_us": row.updated_at_us, "terminal_at_us": row.terminal_at_us,
                }))
            })
        })
        .await
        .unwrap()
}

async fn completed_head_recreate_contract(store: &ToastyUserAppStore) {
    for (scope, action) in [
        (UserAppOperationScope::Dev, ComputeControlAction::Stop),
        (UserAppOperationScope::Dev, ComputeControlAction::Restart),
        (UserAppOperationScope::Prod, ComputeControlAction::Stop),
        (UserAppOperationScope::Prod, ComputeControlAction::Restart),
    ] {
        let app = store
            .ensure_identity(&format!("recreate{}", uuid::Uuid::new_v4().simple()))
            .await
            .unwrap();
        let old = completed_compute(store, &app, scope, action, false).await;
        let old_bytes = raw_control(store, &old.operation_id).await;
        let deletion = delete_application(store, &app).await;
        assert_eq!(
            store
                .get_application(&app.app_id)
                .await
                .unwrap()
                .unwrap()
                .state,
            UserAppLifecycleState::Deleted
        );
        let recreated = store
            .recreate(
                &app.app_id,
                &app.lifecycle_id,
                &uuid::Uuid::new_v4().simple().to_string(),
            )
            .await
            .unwrap();
        assert_ne!(recreated.lifecycle_id, app.lifecycle_id);
        assert_eq!(recreated.lifecycle_epoch, app.lifecycle_epoch + 1);
        assert_eq!(
            store
                .read_compute_status(&app.app_id, &recreated.lifecycle_id, scope)
                .await
                .expect("completed old-life head must not block the new lifecycle status"),
            UserAppComputeStatus::default(),
            "completed old-life head must not block the new lifecycle status"
        );
        assert!(
            !store
                .compute_desired_stopped(&app.app_id, &recreated.lifecycle_id, scope)
                .await
                .unwrap()
        );
        store
            .check_compute_access(&app.app_id, scope, true)
            .await
            .unwrap();
        let business = accepted(
            store
                .admit(&business_request(
                    &recreated,
                    UserAppOperationKind::EnsureBuilder,
                ))
                .await
                .unwrap(),
        );
        assert_eq!(business.lifecycle_id, recreated.lifecycle_id);
        let mut new_request = request(
            &recreated,
            &uuid::Uuid::new_v4().simple().to_string(),
            ComputeControlAction::Restart,
        );
        new_request.scope = scope;
        let new = store.admit_compute_control(&new_request).await.unwrap();
        assert!(
            new.generation > old.generation,
            "new lifecycle cannot reuse a historical generation"
        );
        assert_eq!(new.lifecycle_id, recreated.lifecycle_id);
        assert_eq!(
            raw_control(store, &old.operation_id).await,
            old_bytes,
            "historical receipt/checkpoint/raw fields must remain byte-for-byte unchanged"
        );
        assert_eq!(
            store
                .get_operation(&app.app_id, &deletion.operation_id)
                .await
                .unwrap(),
            Some(deletion)
        );
        assert!(
            store
                .check_compute_executor(&compute_identity(&old))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn completed_compute_heads_do_not_block_recreated_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("recreated-compute.db"))
        .await
        .unwrap();
    completed_head_recreate_contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL database"]
async fn completed_compute_heads_pg_do_not_block_recreated_lifecycle() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        max_connections: Some(1),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    completed_head_recreate_contract(&store).await;
    store.shutdown().await.unwrap();
}

/// A row emitted by the old recreate transaction, restored after the formal
/// lifecycle transition so this remains a regression after new recreates retire it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct IntentFixture {
    app_id: String,
    scope: String,
    lifecycle_id: String,
    generation: i64,
    revision: i64,
    desired_state: String,
    control_operation_id: Option<String>,
    updated_at_us: i64,
}

async fn intent_fixture(
    store: &ToastyUserAppStore,
    app: &str,
    scope: UserAppOperationScope,
) -> Option<IntentFixture> {
    let id = app.to_owned();
    store
        .run(true, move |tx, _| {
            let id = id.clone();
            Box::pin(async move {
                let row = models::ComputeIntent::filter_by_app_id_and_scope(
                    &id,
                    super::super::codec::scope_name(scope),
                )
                .first()
                .exec(tx)
                .await
                .map_err(super::super::storage)?;
                Ok(row.map(|row| IntentFixture {
                    app_id: row.app_id,
                    scope: row.scope,
                    lifecycle_id: row.lifecycle_id,
                    generation: row.generation,
                    revision: row.revision,
                    desired_state: row.desired_state,
                    control_operation_id: row.control_operation_id,
                    updated_at_us: row.updated_at_us,
                }))
            })
        })
        .await
        .unwrap()
}

async fn restore_legacy_intent(store: &ToastyUserAppStore, head: &IntentFixture) {
    let head = head.clone();
    store.run(false, move |tx, backend| {
        let head = head.clone();
        Box::pin(async move {
            toasty::sql::statement(super::super::repo::sql(backend,
                "INSERT INTO userapp_compute_intents (app_id,scope,lifecycle_id,generation,revision,desired_state,control_operation_id,updated_at_us) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(app_id,scope) DO UPDATE SET lifecycle_id=$3,generation=$4,revision=$5,desired_state=$6,control_operation_id=$7,updated_at_us=$8"))
                .bind(&head.app_id).bind(&head.scope).bind(&head.lifecycle_id)
                .bind(head.generation).bind(head.revision).bind(&head.desired_state)
                .bind_typed(head.control_operation_id.clone(), toasty_core::schema::db::Type::Text)
                .bind(head.updated_at_us).exec(tx).await.map_err(super::super::storage)?;
            Ok(())
        })
    }).await.unwrap();
}

async fn recreated_legacy_fixture(
    store: &ToastyUserAppStore,
    scope: UserAppOperationScope,
    action: ComputeControlAction,
    retain_receipt: bool,
) -> (UserAppLifecycleRecord, ComputeControlRecord, IntentFixture) {
    let app = store
        .ensure_identity(&format!("legacy{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let old = completed_compute(store, &app, scope, action, retain_receipt).await;
    let head = intent_fixture(store, &app.app_id, scope).await.unwrap();
    delete_application(store, &app).await;
    let recreated = store
        .recreate(
            &app.app_id,
            &app.lifecycle_id,
            &uuid::Uuid::new_v4().simple().to_string(),
        )
        .await
        .unwrap();
    restore_legacy_intent(store, &head).await;
    (recreated, old, head)
}

async fn legacy_recreated_head_contract(store: &ToastyUserAppStore) {
    for (scope, action) in [
        (UserAppOperationScope::Dev, ComputeControlAction::Stop),
        (UserAppOperationScope::Prod, ComputeControlAction::Restart),
    ] {
        let (recreated, old, head) = recreated_legacy_fixture(store, scope, action, true).await;
        let old_bytes = raw_control(store, &old.operation_id).await;
        let before = store.get_application(&recreated.app_id).await.unwrap();
        assert_eq!(
            store
                .read_compute_status(&recreated.app_id, &recreated.lifecycle_id, scope)
                .await
                .unwrap(),
            UserAppComputeStatus::default()
        );
        assert!(
            !store
                .compute_desired_stopped(&recreated.app_id, &recreated.lifecycle_id, scope)
                .await
                .unwrap()
        );
        store
            .check_compute_access(&recreated.app_id, scope, false)
            .await
            .unwrap();
        assert_eq!(
            intent_fixture(store, &recreated.app_id, scope).await,
            Some(head.clone()),
            "read paths cannot retire or rewrite historical heads"
        );
        assert_eq!(
            store.get_application(&recreated.app_id).await.unwrap(),
            before
        );
        let business_kind = match scope {
            UserAppOperationScope::Dev => UserAppOperationKind::EnsureBuilder,
            UserAppOperationScope::Prod => UserAppOperationKind::Start,
            UserAppOperationScope::Application => panic!("not a compute scope"),
        };
        let pending = accepted(
            store
                .admit(&business_request(&recreated, business_kind))
                .await
                .unwrap(),
        );
        let running = store
            .advance(&business_progress(
                &pending,
                UserAppOperationState::Running,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        let context = UserAppExecutionContext {
            app_id: running.app_id.clone(),
            lifecycle_id: running.lifecycle_id.clone(),
            operation_id: running.operation_id.clone(),
            executor_id: "lifecycleworker".into(),
            request_fingerprint: running.request_fingerprint.clone(),
        };
        store.check_business_execution(&context).await.unwrap();
        store
            .advance(&business_progress(
                &running,
                UserAppOperationState::Succeeded,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        store
            .check_compute_access(&recreated.app_id, scope, true)
            .await
            .unwrap();
        let mut req = request(
            &recreated,
            &uuid::Uuid::new_v4().simple().to_string(),
            ComputeControlAction::Restart,
        );
        req.scope = scope;
        let new = store.admit_compute_control(&req).await.unwrap();
        assert!(new.generation > old.generation);
        assert_eq!(raw_control(store, &old.operation_id).await, old_bytes);
        let new_head = intent_fixture(store, &recreated.app_id, scope).await;
        let receipt = old.lease.clone().unwrap();
        let mut wrong = receipt.clone();
        match &mut wrong {
            UserAppOperationLeaseReceipt::Docker { inode, .. } => *inode += 1,
            UserAppOperationLeaseReceipt::Kubernetes { uid, .. } => uid.push_str("foreign"),
        }
        assert!(
            store
                .forget_compute_lease(&compute_identity(&old), &wrong)
                .await
                .is_err()
        );
        assert_eq!(raw_control(store, &old.operation_id).await, old_bytes);
        store
            .forget_compute_lease(&compute_identity(&old), &receipt)
            .await
            .unwrap();
        let after_cleanup = store
            .get_compute_control(&recreated.app_id, &old.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert!(after_cleanup.lease.is_none());
        assert_eq!(after_cleanup.checkpoint, old.checkpoint);
        assert_eq!(after_cleanup.request_id, old.request_id);
        assert_eq!(after_cleanup.state, old.state);
        assert_eq!(after_cleanup.revision, old.revision + 1);
        assert_eq!(
            intent_fixture(store, &recreated.app_id, scope).await,
            new_head
        );
        assert_eq!(
            store
                .get_compute_control(&recreated.app_id, &new.operation_id)
                .await
                .unwrap(),
            Some(new)
        );
    }
}

#[tokio::test]
async fn legacy_recreated_compute_head_converges_through_original_entry_points() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("legacy-head.db"))
        .await
        .unwrap();
    legacy_recreated_head_contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL database"]
async fn legacy_recreated_compute_head_pg_converges_through_original_entry_points() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        max_connections: Some(1),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    legacy_recreated_head_contract(&store).await;
    store.shutdown().await.unwrap();
}

async fn uncertain_legacy_head_contract(store: &ToastyUserAppStore) {
    for hide_head_target in [false, true] {
        for (state, persisted, stage) in [
            (ComputeControlState::Pending, "pending", "accepted"),
            (ComputeControlState::Running, "running", "starting"),
            (
                ComputeControlState::RecoveryRequired,
                "recovery_required",
                "starting",
            ),
            (ComputeControlState::Superseded, "superseded", "starting"),
        ] {
            let (recreated, old, mut head) = recreated_legacy_fixture(
                store,
                UserAppOperationScope::Dev,
                ComputeControlAction::Restart,
                true,
            )
            .await;
            // Corruption/fault fixture only: old recreate did not validate the compute
            // ledger. A claimed old writer must remain uncertain across a new root.
            let fixture_update = match state {
                ComputeControlState::Pending => {
                    "UPDATE userapp_compute_controls SET state=$2,stage=$3,executor_id=NULL,lease_json=NULL,terminal_at_us=NULL WHERE operation_id=$1"
                }
                ComputeControlState::Running | ComputeControlState::RecoveryRequired => {
                    "UPDATE userapp_compute_controls SET state=$2,stage=$3,terminal_at_us=NULL WHERE operation_id=$1"
                }
                ComputeControlState::Superseded => {
                    "UPDATE userapp_compute_controls SET state=$2,stage=$3 WHERE operation_id=$1"
                }
                ComputeControlState::Succeeded | ComputeControlState::Failed => {
                    panic!("uncertain fixture requires a pending or claimed unknown writer")
                }
            };
            let id = old.operation_id.clone();
            store
                .run(false, move |tx, backend| {
                    let id = id.clone();
                    Box::pin(async move {
                        let changed = toasty::sql::statement(super::super::repo::sql(
                            backend,
                            fixture_update,
                        ))
                        .bind(id)
                        .bind(persisted)
                        .bind(stage)
                        .exec(tx)
                        .await
                        .map_err(super::super::storage)?;
                        assert_eq!(changed, 1, "fixture must update the exact old operation");
                        Ok(())
                    })
                })
                .await
                .unwrap();
            let uncertain = store
                .get_compute_control(&recreated.app_id, &old.operation_id)
                .await
                .expect("fixture must pass actual persisted decoder")
                .expect("fixture operation remains present");
            assert_eq!(uncertain.state, state);
            match state {
                ComputeControlState::Pending => {
                    assert!(uncertain.executor_id.is_none());
                    assert!(uncertain.lease.is_none());
                    assert_eq!(uncertain.stage, "accepted");
                }
                ComputeControlState::Running
                | ComputeControlState::RecoveryRequired
                | ComputeControlState::Superseded => {
                    assert_eq!(uncertain.executor_id, old.executor_id);
                    assert_eq!(uncertain.lease, old.lease);
                    assert_eq!(uncertain.stage, "starting");
                }
                ComputeControlState::Succeeded | ComputeControlState::Failed => {
                    panic!("fixture cannot be a settled terminal operation")
                }
            }
            if hide_head_target {
                head.control_operation_id = None;
                restore_legacy_intent(store, &head).await;
            }
            let old_bytes = raw_control(store, &old.operation_id).await;
            assert!(
                store
                    .read_compute_status(
                        &recreated.app_id,
                        &recreated.lifecycle_id,
                        UserAppOperationScope::Dev
                    )
                    .await
                    .is_err(),
                "uncertain old state must not be hidden: {state:?}"
            );
            assert!(
                store
                    .compute_desired_stopped(
                        &recreated.app_id,
                        &recreated.lifecycle_id,
                        UserAppOperationScope::Dev
                    )
                    .await
                    .is_err()
            );
            assert!(
                store
                    .check_compute_access(&recreated.app_id, UserAppOperationScope::Dev, true)
                    .await
                    .is_err()
            );
            assert!(
                store
                    .admit(&business_request(
                        &recreated,
                        UserAppOperationKind::EnsureBuilder
                    ))
                    .await
                    .is_err()
            );
            let new_req = request(
                &recreated,
                &uuid::Uuid::new_v4().simple().to_string(),
                ComputeControlAction::Restart,
            );
            assert!(store.admit_compute_control(&new_req).await.is_err());
            assert!(
                store
                    .get_compute_control(&recreated.app_id, &new_req.operation_id)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                intent_fixture(store, &recreated.app_id, UserAppOperationScope::Dev).await,
                Some(head)
            );
            assert_eq!(raw_control(store, &old.operation_id).await, old_bytes);
            assert_eq!(
                store.get_application(&recreated.app_id).await.unwrap(),
                Some(recreated)
            );
        }
    }
}

#[tokio::test]
async fn uncertain_old_compute_writes_cannot_be_bypassed_by_recreated_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("uncertain-head.db"))
        .await
        .unwrap();
    uncertain_legacy_head_contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL database"]
async fn uncertain_old_compute_writes_pg_cannot_be_bypassed_by_recreated_lifecycle() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        max_connections: Some(1),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    uncertain_legacy_head_contract(&store).await;
    store.shutdown().await.unwrap();
}

async fn older_recreated_head_without_mapping_contract(store: &ToastyUserAppStore) {
    let first = store
        .ensure_identity(&format!("older{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let first_compute = completed_compute(
        store,
        &first,
        UserAppOperationScope::Dev,
        ComputeControlAction::Stop,
        false,
    )
    .await;
    let first_head = intent_fixture(store, &first.app_id, UserAppOperationScope::Dev)
        .await
        .unwrap();
    let first_raw = raw_control(store, &first_compute.operation_id).await;
    let first_deletion = delete_application(store, &first).await;
    let second = store
        .recreate(
            &first.app_id,
            &first.lifecycle_id,
            &uuid::Uuid::new_v4().simple().to_string(),
        )
        .await
        .unwrap();
    let second_compute = completed_compute(
        store,
        &second,
        UserAppOperationScope::Dev,
        ComputeControlAction::Restart,
        false,
    )
    .await;
    assert!(second_compute.generation > first_compute.generation);
    let second_raw = raw_control(store, &second_compute.operation_id).await;
    let second_deletion = delete_application(store, &second).await;
    let third = store
        .recreate(
            &second.app_id,
            &second.lifecycle_id,
            &uuid::Uuid::new_v4().simple().to_string(),
        )
        .await
        .unwrap();
    assert_eq!(third.lifecycle_epoch, first.lifecycle_epoch + 2);
    // An old head may predate several recreates. Optional request mappings are
    // not physical-write authority and cannot become a new permanent gate.
    restore_legacy_intent(store, &first_head).await;
    let app_id = first.app_id.clone();
    store
        .run(false, move |tx, backend| {
            let app_id = app_id.clone();
            Box::pin(async move {
                let deleted = toasty::sql::statement(super::super::repo::sql(
                    backend,
                    "DELETE FROM userapp_requests WHERE app_id=$1 AND target_kind='recreate'",
                ))
                .bind(app_id)
                .exec(tx)
                .await
                .map_err(super::super::storage)?;
                assert_eq!(
                    deleted, 2,
                    "fixture removes only the two recreate request mappings"
                );
                Ok(())
            })
        })
        .await
        .unwrap();
    for old_life in [&first.lifecycle_id, &second.lifecycle_id] {
        assert!(matches!(
            store
                .read_compute_status(&first.app_id, old_life, UserAppOperationScope::Dev)
                .await,
            Err(UserAppStoreError::LifecycleConflict)
        ));
        assert!(matches!(
            store
                .compute_desired_stopped(&first.app_id, old_life, UserAppOperationScope::Dev)
                .await,
            Err(UserAppStoreError::LifecycleConflict)
        ));
    }
    assert_eq!(
        store
            .read_compute_status(
                &first.app_id,
                &third.lifecycle_id,
                UserAppOperationScope::Dev
            )
            .await
            .unwrap(),
        UserAppComputeStatus::default()
    );
    assert!(
        !store
            .compute_desired_stopped(
                &first.app_id,
                &third.lifecycle_id,
                UserAppOperationScope::Dev
            )
            .await
            .unwrap()
    );
    store
        .check_compute_access(&first.app_id, UserAppOperationScope::Dev, false)
        .await
        .unwrap();
    assert_eq!(
        intent_fixture(store, &first.app_id, UserAppOperationScope::Dev).await,
        Some(first_head)
    );
    let pending = accepted(
        store
            .admit(&business_request(
                &third,
                UserAppOperationKind::EnsureBuilder,
            ))
            .await
            .unwrap(),
    );
    let running = store
        .advance(&business_progress(
            &pending,
            UserAppOperationState::Running,
            serde_json::Value::Null,
        ))
        .await
        .unwrap();
    store
        .advance(&business_progress(
            &running,
            UserAppOperationState::Succeeded,
            serde_json::Value::Null,
        ))
        .await
        .unwrap();
    let new = store
        .admit_compute_control(&request(
            &third,
            &uuid::Uuid::new_v4().simple().to_string(),
            ComputeControlAction::Restart,
        ))
        .await
        .unwrap();
    assert!(new.generation > second_compute.generation);
    assert_eq!(
        raw_control(store, &first_compute.operation_id).await,
        first_raw
    );
    assert_eq!(
        raw_control(store, &second_compute.operation_id).await,
        second_raw
    );
    assert_eq!(
        store
            .get_operation(&first.app_id, &first_deletion.operation_id)
            .await
            .unwrap(),
        Some(first_deletion)
    );
    assert_eq!(
        store
            .get_operation(&first.app_id, &second_deletion.operation_id)
            .await
            .unwrap(),
        Some(second_deletion)
    );
}

#[tokio::test]
async fn older_recreated_compute_head_needs_no_request_mapping_chain() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("older-head.db"))
        .await
        .unwrap();
    older_recreated_head_without_mapping_contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL database"]
async fn older_recreated_compute_head_pg_needs_no_request_mapping_chain() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        max_connections: Some(1),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    older_recreated_head_without_mapping_contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL database"]
async fn explicit_compute_access_pg_reads_head_after_root_claim() {
    use super::super::transaction_fault_tests::{
        ComputeAccessClaimProbe, with_compute_access_claim_probe,
    };
    use std::{sync::Arc, time::Duration};
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        max_connections: Some(1),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    // Independent owners, each with exactly one physical connection. The paused
    // reader never prevents the writer owner from running its own transactions.
    let reader = Arc::new(ToastyUserAppStore::connect(&config).await.unwrap());
    let writer = ToastyUserAppStore::connect(&config).await.unwrap();
    let app = writer
        .ensure_identity(&format!("accessrace{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let old = completed_compute(
        &writer,
        &app,
        UserAppOperationScope::Dev,
        ComputeControlAction::Restart,
        false,
    )
    .await;
    let old_raw = raw_control(&writer, &old.operation_id).await;
    let original_head = intent_fixture(&writer, &app.app_id, UserAppOperationScope::Dev)
        .await
        .unwrap();
    let probe = Arc::new(ComputeAccessClaimProbe::default());
    let access = {
        let reader = Arc::clone(&reader);
        let probe = Arc::clone(&probe);
        let app_id = app.app_id.clone();
        tokio::spawn(async move {
            // Same actual read-committed store transaction as the public
            // explicit-start entry, scoped inside the owner task for isolation.
            reader
                .run(false, move |tx, backend| {
                    let (probe, app_id) = (Arc::clone(&probe), app_id.clone());
                    Box::pin(with_compute_access_claim_probe(probe, async move {
                        super::super::compute::check_access(
                            tx,
                            backend,
                            &app_id,
                            UserAppOperationScope::Dev,
                            true,
                        )
                        .await
                    }))
                })
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), probe.reached())
        .await
        .expect("reader must reach the deterministic pre-root-claim boundary");
    assert_eq!(
        intent_fixture(&writer, &app.app_id, UserAppOperationScope::Dev).await,
        Some(original_head)
    );
    let deletion = delete_application(&writer, &app).await;
    let recreated = writer
        .recreate(
            &app.app_id,
            &app.lifecycle_id,
            &uuid::Uuid::new_v4().simple().to_string(),
        )
        .await
        .unwrap();
    let new = writer
        .admit_compute_control(&request(
            &recreated,
            &uuid::Uuid::new_v4().simple().to_string(),
            ComputeControlAction::Stop,
        ))
        .await
        .unwrap();
    assert_eq!(new.state, ComputeControlState::Pending);
    assert_ne!(new.lifecycle_id, old.lifecycle_id);
    assert!(new.generation > old.generation);
    // Both recreation and the new Stop have committed before the reader is
    // allowed to acquire the root. An old H0 must not authorize access to H1.
    probe.release();
    let result = tokio::time::timeout(Duration::from_secs(10), access)
        .await
        .expect("access transaction must finish")
        .expect("access task must not panic");
    match result {
        Err(UserAppStoreError::OperationInProgress(blocker)) => {
            assert_eq!(blocker.operation_id, new.operation_id);
            assert_eq!(blocker.scope, UserAppOperationScope::Dev);
            assert_eq!(blocker.kind, UserAppOperationKind::StopBuilder);
        }
        Err(error) => panic!("must observe the current Stop identity, got {error:?}"),
        Ok(()) => panic!("stale pre-claim head incorrectly bypassed the current Stop"),
    }
    assert_eq!(
        writer
            .read_compute_status(
                &app.app_id,
                &recreated.lifecycle_id,
                UserAppOperationScope::Dev
            )
            .await
            .unwrap()
            .operation,
        Some(new.clone())
    );
    assert_eq!(
        writer
            .get_compute_control(&app.app_id, &new.operation_id)
            .await
            .unwrap(),
        Some(new)
    );
    assert_eq!(raw_control(&writer, &old.operation_id).await, old_raw);
    assert_eq!(
        writer
            .get_operation(&app.app_id, &deletion.operation_id)
            .await
            .unwrap(),
        Some(deletion)
    );
    reader.shutdown().await.unwrap();
    writer.shutdown().await.unwrap();
}

async fn missing_root_compute_observation_contract(store: &ToastyUserAppStore) {
    for scope in [UserAppOperationScope::Dev, UserAppOperationScope::Prod] {
        let app_id = format!("absent{}", uuid::Uuid::new_v4().simple());
        let lifecycle_id = "notcreatedlifecycle";
        assert!(store.get_application(&app_id).await.unwrap().is_none());
        assert!(
            !store
                .compute_desired_stopped(&app_id, lifecycle_id, scope)
                .await
                .unwrap()
        );
        for explicit in [false, true] {
            store
                .check_compute_access(&app_id, scope, explicit)
                .await
                .unwrap();
        }
        assert!(
            store.get_application(&app_id).await.unwrap().is_none(),
            "read-only absence must not create a lifecycle root"
        );
        assert!(
            intent_fixture(store, &app_id, scope).await.is_none(),
            "read-only absence must not create a compute head"
        );
        // A dangling pointer is not legitimate absence and must not authorize
        // traffic. This narrowly owned fixture uses the actual unchanged schema.
        let dangling = IntentFixture {
            app_id: app_id.clone(),
            scope: super::super::codec::scope_name(scope).into(),
            lifecycle_id: lifecycle_id.into(),
            generation: 1,
            revision: 1,
            desired_state: "running".into(),
            control_operation_id: None,
            updated_at_us: chrono::Utc::now().timestamp_micros(),
        };
        restore_legacy_intent(store, &dangling).await;
        assert!(
            store
                .compute_desired_stopped(&app_id, lifecycle_id, scope)
                .await
                .is_err(),
            "a dangling head cannot be interpreted as a fresh application's intent"
        );
        for explicit in [false, true] {
            assert!(
                store
                    .check_compute_access(&app_id, scope, explicit)
                    .await
                    .is_err(),
                "missing root plus a head must reject access"
            );
        }
        assert!(store.get_application(&app_id).await.unwrap().is_none());
        assert_eq!(
            intent_fixture(store, &app_id, scope).await,
            Some(dangling),
            "failed dangling-pointer observations must not mutate the pointer"
        );
    }
}

#[tokio::test]
async fn missing_root_compute_observation_preserves_absence_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("absent-root.db"))
        .await
        .unwrap();
    missing_root_compute_observation_contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL database"]
async fn missing_root_compute_observation_pg_preserves_absence_without_writes() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        max_connections: Some(1),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    missing_root_compute_observation_contract(&store).await;
    store.shutdown().await.unwrap();
}
