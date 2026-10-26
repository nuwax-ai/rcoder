//! Each fault is local to one owned transaction, never another concurrent test.
use super::{ToastyUserAppStore, ops, repo};
use shared_types::*;
tokio::task_local! { static POINT: &'static str; }
pub(super) fn check(point: &str) -> Result<(), UserAppStoreError> {
    if POINT
        .try_with(|expected| *expected == point)
        .unwrap_or(false)
    {
        return Err(UserAppStoreError::InvalidOperation(format!(
            "injected:{point}"
        )));
    }
    Ok(())
}
/// Only the owning test transaction can pause here. Notify permits make the
/// schedule explicit; no timing-based assumption is used to trigger the race.
#[derive(Default)]
pub(super) struct ComputeAccessClaimProbe {
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}
impl ComputeAccessClaimProbe {
    #[cfg(all(feature = "pg", feature = "userapp-turso"))]
    pub(super) async fn reached(&self) {
        self.reached.notified().await;
    }
    #[cfg(all(feature = "pg", feature = "userapp-turso"))]
    pub(super) fn release(&self) {
        self.resume.notify_one();
    }
}
tokio::task_local! {
    static COMPUTE_ACCESS_CLAIM_PROBE: std::sync::Arc<ComputeAccessClaimProbe>;
}
#[cfg(all(feature = "pg", feature = "userapp-turso"))]
pub(super) async fn with_compute_access_claim_probe<F: Future>(
    probe: std::sync::Arc<ComputeAccessClaimProbe>,
    future: F,
) -> F::Output {
    COMPUTE_ACCESS_CLAIM_PROBE.scope(probe, future).await
}
/// Only the owning test transaction can pause here: the expire path holds
/// the anchor row lock and pauses between the lock acquisition and the
/// terminal CAS, letting a concurrency test prove a concurrent clear cannot
/// slip into that gap.
#[derive(Default)]
pub(super) struct ExpireAnchorProbe {
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}
impl ExpireAnchorProbe {
    #[cfg(feature = "pg")]
    pub(super) async fn reached(&self) {
        self.reached.notified().await;
    }
    #[cfg(feature = "pg")]
    pub(super) fn release(&self) {
        self.resume.notify_one();
    }
}
tokio::task_local! {
    static EXPIRE_ANCHOR_PROBE: std::sync::Arc<ExpireAnchorProbe>;
}
#[cfg(feature = "pg")]
pub(super) async fn with_expire_anchor_probe<F: Future>(
    probe: std::sync::Arc<ExpireAnchorProbe>,
    future: F,
) -> F::Output {
    EXPIRE_ANCHOR_PROBE.scope(probe, future).await
}
#[cfg(feature = "pg")]
pub(super) async fn after_expire_anchor_lock() {
    let probe = EXPIRE_ANCHOR_PROBE.try_with(Clone::clone).ok();
    if let Some(probe) = probe {
        probe.reached.notify_one();
        probe.resume.notified().await;
    }
}

pub(super) async fn before_compute_access_root_claim() {
    let probe = COMPUTE_ACCESS_CLAIM_PROBE.try_with(Clone::clone).ok();
    if let Some(probe) = probe {
        probe.reached.notify_one();
        probe.resume.notified().await;
    }
}
fn accepted(value: UserAppAdmissionOutcome) -> UserAppOperationRecord {
    match value {
        UserAppAdmissionOutcome::Accepted(op) => op,
        UserAppAdmissionOutcome::Existing(op) => panic!("fresh admission required: {op:?}"),
    }
}
fn request(app: &UserAppLifecycleRecord, kind: UserAppOperationKind) -> UserAppAdmission {
    UserAppAdmission {
        app_id: app.app_id.clone(),
        lifecycle_id: Some(app.lifecycle_id.clone()),
        operation_id: uuid::Uuid::new_v4().simple().to_string(),
        request_id: Some(uuid::Uuid::new_v4().simple().to_string()),
        request_fingerprint: "a".repeat(64),
        kind,
        command: None,
        metadata: None,
        runtime_policy_on_success: None,
    }
}
fn progress(
    op: &UserAppOperationRecord,
    state: UserAppOperationState,
    checkpoint: serde_json::Value,
) -> UserAppOperationProgress {
    UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: "faultworker".into(),
        state,
        step: "fixture".into(),
        checkpoint,
        error_code: None,
        error_message: None,
    }
}
async fn empty_children(store: &ToastyUserAppStore, app: &str) {
    let app = app.to_owned();
    let backend = store.backend;
    store
        .owner
        .execute(move |mut db| async move {
            for table in [
                "userapp_operations",
                "userapp_requests",
                "userapp_operation_inputs",
            ] {
                let rows = toasty::sql::query(repo::sql(
                    backend,
                    &format!("SELECT app_id FROM {table} WHERE app_id=$1"),
                ))
                .bind(&app)
                .exec(&mut db)
                .await?;
                assert!(rows.is_empty(), "{table} must roll back");
            }
            Ok(())
        })
        .await
        .unwrap();
}
async fn completed_compute_before_recreate(
    store: &ToastyUserAppStore,
    app: &UserAppLifecycleRecord,
) -> ComputeControlRecord {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let pending = store
        .admit_compute_control(&ComputeControlRequest {
            builder_startup_mode: Default::default(),
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: id.clone(),
            request_id: id,
            request_fingerprint: "c".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: UserAppOperationScope::Dev,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "faultcompute".into(),
    };
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let receipt = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: 1,
        inode: 42,
        token: "faultfixturetoken".into(),
    };
    let mut record = store.bind_compute_lease(&identity, &receipt).await.unwrap();
    for stage in [
        ComputeControlStage::Stopping,
        ComputeControlStage::Stopped,
        ComputeControlStage::Completed,
    ] {
        record = store
            .advance_compute_control(&ComputeControlProgress {
                identity: identity.clone(),
                expected_revision: record.revision,
                state: match stage {
                    ComputeControlStage::Completed => ComputeControlState::Succeeded,
                    ComputeControlStage::DrainingPrevious
                    | ComputeControlStage::Stopping
                    | ComputeControlStage::Stopped
                    | ComputeControlStage::Starting
                    | ComputeControlStage::Verifying => ComputeControlState::Running,
                },
                stage,
                checkpoint: serde_json::json!({"fixture":"byte-preserved-history"}),
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
    }
    record
}
async fn compute_head_and_raw_receipts(store: &ToastyUserAppStore, app_id: &str) -> Vec<String> {
    let id = app_id.to_owned();
    store.run(true, move |tx, backend| {
        let id = id.clone();
        Box::pin(async move {
            repo::strings(toasty::sql::query(repo::sql(backend,
                "SELECT lifecycle_id || '|' || scope || '|' || CAST(generation AS TEXT) || '|' || CAST(revision AS TEXT) || '|' || desired_state || '|' || COALESCE(control_operation_id,'null') || '|' || CAST(updated_at_us AS TEXT) FROM userapp_compute_intents WHERE app_id=$1 UNION ALL SELECT checkpoint_json || '|' || COALESCE(lease_json,'null') || '|' || evidence_json FROM userapp_compute_controls WHERE app_id=$1 ORDER BY 1"))
                .bind(id).exec(tx).await.map_err(super::storage)?)
        })
    }).await.unwrap()
}
async fn contract(store: &ToastyUserAppStore) {
    for point in [
        "admission_operation",
        "admission_input",
        "admission_application",
        "admission_slots",
        "admission_request",
    ] {
        let before = store
            .ensure_identity(&format!("fault{}", uuid::Uuid::new_v4().simple()))
            .await
            .unwrap();
        let input = UserAppExecutionInput::new("{\"fixture\":\"private\"}".into());
        let mut req = request(&before, UserAppOperationKind::Create);
        req.request_fingerprint = input.digest();
        req.command = Some(UserAppControlCommand::Create {
            input_digest: input.digest(),
        });
        let (captured, payload) = (req.clone(), input.clone());
        let result = store
            .run(false, move |tx, backend| {
                let (captured, payload) = (captured.clone(), payload.clone());
                Box::pin(POINT.scope(point, async move {
                    ops::admit_with_input(tx, backend, &captured, Some(&payload)).await
                }))
            })
            .await;
        assert!(
            matches!(result, Err(UserAppStoreError::InvalidOperation(ref message)) if message == &format!("injected:{point}")),
            "fault must actually execute: {point}"
        );
        assert_eq!(
            store
                .get_application(&before.app_id)
                .await
                .unwrap()
                .unwrap(),
            before
        );
        empty_children(store, &before.app_id).await;
        let op = accepted(store.admit_with_input(&req, Some(&input)).await.unwrap());
        assert_eq!(op.operation_id, req.operation_id);
        assert!(
            matches!(store.admit_with_input(&req, Some(&input)).await.unwrap(), UserAppAdmissionOutcome::Existing(ref same) if same.operation_id == op.operation_id)
        );
    }
    for point in [
        "recreate_compute_intents",
        "recreate_deleted_slots",
        "recreate_application",
        "recreate_slots",
        "recreate_request",
    ] {
        let app = store
            .ensure_identity(&format!("recreate{}", uuid::Uuid::new_v4().simple()))
            .await
            .unwrap();
        let compute = completed_compute_before_recreate(store, &app).await;
        let original_compute_rows = compute_head_and_raw_receipts(store, &app.app_id).await;
        let req = request(&app, UserAppOperationKind::DeleteApplication);
        let mut op = accepted(store.admit(&req).await.unwrap());
        op = store
            .advance(&progress(
                &op,
                UserAppOperationState::Running,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        let mut evidence = UserAppDeletionCheckpoint {
            schema_version: 1,
            stage: UserAppDeletionStage::Captured,
            kind: op.kind,
            context: UserAppExecutionContext {
                app_id: op.app_id.clone(),
                lifecycle_id: op.lifecycle_id.clone(),
                operation_id: op.operation_id.clone(),
                executor_id: "faultworker".into(),
                request_fingerprint: op.request_fingerprint.clone(),
            },
            production: AppDeletionSnapshot {
                app_id: app.app_id.clone(),
                operation_id: "fixtureprod".into(),
                resources: vec![],
                directories: None,
            },
            development: Some(UserappDevDeletionReceipt {
                runtime: BuilderDeletionSnapshot {
                    resource_binding: None,
                    app_id: app.app_id.clone(),
                    operation_id: "fixturedev".into(),
                    resources: vec![],
                    docker_bind_cleanup: false,
                },
                registry: None,
                lease: None,
                collaborators: Vec::new(),
                directories: None,
            }),
        };
        for stage in [
            UserAppDeletionStage::Captured,
            UserAppDeletionStage::ComputeRemoved,
            UserAppDeletionStage::ProductionStorageRemoved,
            UserAppDeletionStage::DevelopmentRemoved,
        ] {
            evidence.stage = stage;
            op = store
                .advance(&progress(
                    &op,
                    UserAppOperationState::Running,
                    serde_json::to_value(&evidence).unwrap(),
                ))
                .await
                .unwrap();
        }
        op = store
            .advance(&progress(
                &op,
                UserAppOperationState::Succeeded,
                op.checkpoint.clone(),
            ))
            .await
            .unwrap();
        let before = store.get_application(&app.app_id).await.unwrap().unwrap();
        assert_eq!(before.state, UserAppLifecycleState::Deleted);
        let (id, life) = (app.app_id.clone(), app.lifecycle_id.clone());
        let retry = uuid::Uuid::new_v4().simple().to_string();
        let retry_capture = retry.clone();
        let result = store
            .run(false, move |tx, backend| {
                let (id, life, retry) = (id.clone(), life.clone(), retry_capture.clone());
                Box::pin(POINT.scope(point, async move {
                    ops::recreate(tx, backend, &id, &life, &retry).await
                }))
            })
            .await;
        assert!(
            matches!(result, Err(UserAppStoreError::InvalidOperation(ref message)) if message == &format!("injected:{point}")),
            "fault must actually execute: {point}"
        );
        assert_eq!(
            store.get_application(&app.app_id).await.unwrap().unwrap(),
            before
        );
        assert_eq!(
            store
                .get_operation(&app.app_id, &op.operation_id)
                .await
                .unwrap()
                .unwrap(),
            op
        );
        assert_eq!(
            compute_head_and_raw_receipts(store, &app.app_id).await,
            original_compute_rows,
            "head retirement and every raw checkpoint/receipt must roll back: {point}"
        );
        assert_eq!(
            store
                .get_compute_control(&app.app_id, &compute.operation_id)
                .await
                .unwrap(),
            Some(compute.clone())
        );
        let (id, expected, retry_check) =
            (app.app_id.clone(), app.lifecycle_id.clone(), retry.clone());
        let backend = store.backend;
        store
            .owner
            .execute(move |mut db| async move {
                let rows = toasty::sql::query(repo::sql(
                    backend,
                    "SELECT lifecycle_id FROM userapp_active_operations WHERE app_id=$1",
                ))
                .bind(&id)
                .exec(&mut db)
                .await?;
                assert_eq!(repo::strings(rows).unwrap(), vec![expected]);
                let rows = toasty::sql::query(repo::sql(
                    backend,
                    "SELECT app_id FROM userapp_requests WHERE app_id=$1 AND request_id=$2",
                ))
                .bind(id)
                .bind(retry_check)
                .exec(&mut db)
                .await?;
                assert!(rows.is_empty());
                Ok(())
            })
            .await
            .unwrap();
        let after = store
            .recreate(&app.app_id, &app.lifecycle_id, &retry)
            .await
            .unwrap();
        assert_ne!(after.lifecycle_id, before.lifecycle_id);
        assert_eq!(
            store
                .get_compute_control(&app.app_id, &compute.operation_id)
                .await
                .unwrap(),
            Some(compute)
        );
        assert_eq!(
            store
                .read_compute_status(&app.app_id, &after.lifecycle_id, UserAppOperationScope::Dev)
                .await
                .unwrap(),
            UserAppComputeStatus::default()
        );
        assert_eq!(after.lifecycle_epoch, before.lifecycle_epoch + 1);
        assert_eq!(
            store
                .get_operation(&app.app_id, &op.operation_id)
                .await
                .unwrap()
                .unwrap(),
            op
        );
        assert_eq!(
            store
                .recreate(&app.app_id, &app.lifecycle_id, &retry)
                .await
                .unwrap(),
            after
        );
    }
}
#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn turso_each_admission_and_recreate_write_fault_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&dir.path().join("faults.db"))
        .await
        .unwrap();
    contract(&store).await;
    store.shutdown().await.unwrap();
}
#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires isolated RCODER_USERAPP_PG_TEST_DSN"]
async fn pg_each_admission_and_recreate_write_fault_rolls_back() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("isolated PG DSN required")),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    contract(&store).await;
    store.shutdown().await.unwrap();
}
