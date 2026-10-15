//! Real PostgreSQL root-lock contention, without retry wrappers masking a stale token.
use super::{Backend, claim_app};
use crate::db::owner::DatabaseOwner;
use crate::userapp_lifecycle::common::ToastyUserAppStore;
use shared_types::*;
use std::time::{Duration, Instant};

async fn fixture() -> (ToastyUserAppStore, String, UserAppLifecycleRecord) {
    let dsn = std::env::var("RCODER_USERAPP_PG_TEST_DSN")
        .expect("explicit disposable PostgreSQL DSN required");
    let store = ToastyUserAppStore::connect(&crate::config::PostgresConfig {
        url: Some(dsn.clone()),
        max_connections: Some(2),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    })
    .await
    .unwrap();
    let app = store
        .ensure_identity(&format!("rootclaim{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    (store, dsn, app)
}

async fn raw_claim(owner: DatabaseOwner, app_id: String) -> Result<(), UserAppStoreError> {
    owner
        .execute(move |mut db| async move {
            let mut tx = db.transaction().await?;
            // One production claim only. A retrying store/registry wrapper
            // would hide the old SELECT/UPDATE race this test must expose.
            let result = claim_app(&mut tx, Backend::Postgres, &app_id).await;
            if result.is_ok() {
                tx.commit().await?;
            } else {
                tx.rollback().await?;
            }
            Ok(result)
        })
        .await
        .unwrap()
}

async fn wait_for_root_lock_wait(writer: &tokio_postgres::Transaction<'_>, writer_pid: i32) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let blocked: bool = writer
                .query_one(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid)))",
                    &[&writer_pid],
                )
                .await
                .unwrap()
                .get(0);
            if blocked {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("production root claim must reach the actual PostgreSQL row lock");
}

#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL test database"]
async fn pg_root_claim_reads_the_current_token_after_real_row_lock_contention() {
    let (store, dsn, app) = fixture().await;
    let (mut writer, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let writer_pid: i32 = writer
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    for _round in 0..3 {
        let competing = writer.transaction().await.unwrap();
        let before: i64 = competing
            .query_one(
                "SELECT control_revision FROM userapps WHERE app_id=$1 FOR UPDATE",
                &[&app.app_id],
            )
            .await
            .unwrap()
            .get(0);
        let release_writer = async {
            wait_for_root_lock_wait(&competing, writer_pid).await;
            assert_eq!(
                competing
                    .execute(
                        "UPDATE userapps SET control_revision=control_revision+1 WHERE app_id=$1",
                        &[&app.app_id]
                    )
                    .await
                    .unwrap(),
                1
            );
            competing.commit().await.unwrap();
        };
        let (claim, ()) = tokio::join!(
            raw_claim(store.owner.clone(), app.app_id.clone()),
            release_writer
        );
        claim.expect("one root claim must read the committed token after acquiring its lock");
        let after: i64 = writer
            .query_one(
                "SELECT control_revision FROM userapps WHERE app_id=$1",
                &[&app.app_id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            after,
            before + 2,
            "one competing increment and one committed claim"
        );
        assert_eq!(
            store.get_application(&app.app_id).await.unwrap(),
            Some(app.clone())
        );
    }
    store.shutdown().await.unwrap();
    connection.abort();
}

#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL test database"]
async fn pg_root_claim_keeps_the_existing_lock_timeout_and_rolls_back() {
    let (store, dsn, app) = fixture().await;
    let (mut writer, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let writer_pid: i32 = writer
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let competing = writer.transaction().await.unwrap();
    let before: i64 = competing
        .query_one(
            "SELECT control_revision FROM userapps WHERE app_id=$1 FOR UPDATE",
            &[&app.app_id],
        )
        .await
        .unwrap()
        .get(0);
    let started = Instant::now();
    let claim = tokio::spawn(raw_claim(store.owner.clone(), app.app_id.clone()));
    wait_for_root_lock_wait(&competing, writer_pid).await;
    let error = claim.await.unwrap().unwrap_err();
    assert!(
        matches!(error, UserAppStoreError::Storage(_)),
        "an actual SQL lock timeout stays a storage failure"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "root locking must keep the configured two-second lock budget"
    );
    let after: i64 = competing
        .query_one(
            "SELECT control_revision FROM userapps WHERE app_id=$1",
            &[&app.app_id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        after, before,
        "failed lock acquisition cannot advance the root"
    );
    competing.rollback().await.unwrap();
    assert_eq!(store.get_application(&app.app_id).await.unwrap(), Some(app));
    store.shutdown().await.unwrap();
    connection.abort();
}

#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL test database"]
async fn pg_locked_root_retains_child_cas_scope_executor_and_exact_lease_guards() {
    let (store, _dsn, app) = fixture().await;
    let pending = store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: uuid::Uuid::new_v4().to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            request_fingerprint: "a".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
            builder_startup_mode: Default::default(),
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "root-guard-worker".into(),
    };
    let claimed = store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    assert!(matches!(
        store
            .claim_compute_control(&identity, pending.revision)
            .await,
        Err(UserAppStoreError::VersionConflict)
    ));
    let receipt = UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "root-guard-fixture".into(),
        name: "root-guard-lease".into(),
        uid: "root-guard-lease-uid".into(),
        resource_version: "1".into(),
        token: "root-guard-token".into(),
    };
    let mut record = store.bind_compute_lease(&identity, &receipt).await.unwrap();
    for foreign in 0..3 {
        let mut forged = identity.clone();
        match foreign {
            0 => forged.scope = UserAppOperationScope::Prod,
            1 => forged.lifecycle_id = "foreign-lifecycle".into(),
            2 => forged.executor_id = "foreign-worker".into(),
            _ => unreachable!(),
        }
        assert!(store.check_compute_executor(&forged).await.is_err());
        assert!(store.bind_compute_lease(&forged, &receipt).await.is_err());
        assert_eq!(
            store
                .get_compute_control(&app.app_id, &pending.operation_id)
                .await
                .unwrap(),
            Some(record.clone())
        );
    }
    let mut wrong_receipt = receipt.clone();
    if let UserAppOperationLeaseReceipt::Kubernetes { uid, .. } = &mut wrong_receipt {
        *uid = "foreign-lease-uid".into();
    }
    assert!(
        store
            .bind_compute_lease(&identity, &wrong_receipt)
            .await
            .is_err()
    );
    let checkpoint = serde_json::to_value(ComputeAbsenceCheckpoint {
        context: record.execution_context().unwrap(),
        compute_absent: true,
    })
    .unwrap();
    let stale = ComputeControlProgress {
        identity: identity.clone(),
        expected_revision: claimed.revision,
        state: ComputeControlState::Running,
        stage: ComputeControlStage::Stopping,
        checkpoint: checkpoint.clone(),
        error_code: None,
        error_message: None,
    };
    assert!(matches!(
        store.advance_compute_control(&stale).await,
        Err(UserAppStoreError::VersionConflict)
    ));
    assert_eq!(
        store
            .get_compute_control(&app.app_id, &pending.operation_id)
            .await
            .unwrap(),
        Some(record.clone())
    );
    for stage in [
        ComputeControlStage::Stopping,
        ComputeControlStage::Stopped,
        ComputeControlStage::Completed,
    ] {
        let progress = ComputeControlProgress {
            identity: identity.clone(),
            expected_revision: record.revision,
            state: if stage == ComputeControlStage::Completed {
                ComputeControlState::Succeeded
            } else {
                ComputeControlState::Running
            },
            stage,
            checkpoint: checkpoint.clone(),
            error_code: None,
            error_message: None,
        };
        record = store.advance_compute_control(&progress).await.unwrap();
        assert!(matches!(
            store.advance_compute_control(&progress).await,
            Err(UserAppStoreError::VersionConflict)
        ));
    }
    assert!(
        store
            .forget_compute_lease(&identity, &wrong_receipt)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .get_compute_control(&app.app_id, &pending.operation_id)
            .await
            .unwrap(),
        Some(record.clone())
    );
    store
        .forget_compute_lease(&identity, &receipt)
        .await
        .unwrap();
    let final_record = store
        .get_compute_control(&app.app_id, &pending.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert!(final_record.lease.is_none());
    assert_eq!(final_record.state, ComputeControlState::Succeeded);
    store.shutdown().await.unwrap();
}
