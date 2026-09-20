use super::owner::DatabaseOwner;
#[derive(toasty::Model)]
struct Probe {
    #[key]
    id: i64,
    revision: i64,
    value: String,
}
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

struct Resource(Arc<AtomicBool>);
impl Drop for Resource {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
async fn owner() -> (DatabaseOwner, Arc<AtomicBool>) {
    let dropped = Arc::new(AtomicBool::new(false));
    let db = DatabaseOwner::open(8, 1, Resource(dropped.clone()), || async {
        let db = toasty::Db::builder()
            .models(toasty::models!(Probe))
            .max_pool_size(1)
            .connect("turso::memory:")
            .await?;
        db.push_schema().await?;
        Ok(db)
    })
    .await
    .unwrap();
    (db, dropped)
}

#[tokio::test]
async fn cancelled_caller_does_not_cancel_admitted_transaction() {
    let (owner, dropped) = owner().await;
    let (entered, started) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    let request = tokio::spawn({
        let owner = owner.clone();
        async move {
            owner
                .execute(move |mut db| async move {
                    let mut tx = db.transaction().await?;
                    Probe::create()
                        .id(1)
                        .revision(0)
                        .value("committed")
                        .exec(&mut tx)
                        .await?;
                    entered.send(()).unwrap();
                    resumed.await?;
                    tx.commit().await?;
                    Ok(())
                })
                .await
        }
    });
    started.await.unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert!(!dropped.load(Ordering::SeqCst));
    resume.send(()).unwrap();
    let value = owner
        .execute(|mut db| async move { Ok(Probe::get_by_id(&mut db, 1).await?.value) })
        .await
        .unwrap();
    assert_eq!(value, "committed");
    owner.shutdown().await.unwrap();
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn shutdown_waiter_cancellation_keeps_lock_until_jobs_and_runtime_end() {
    let (owner, dropped) = owner().await;
    let (entered, started) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    let request = tokio::spawn({
        let owner = owner.clone();
        async move {
            owner
                .execute(move |_| async move {
                    entered.send(()).unwrap();
                    resumed.await?;
                    Ok(())
                })
                .await
        }
    });
    started.await.unwrap();
    // Poll shutdown until pending: admission must already be closed.
    let mut shutdown = Box::pin(owner.shutdown());
    assert!(futures::poll!(&mut shutdown).is_pending());
    drop(shutdown);
    assert!(owner.execute(|_| async { Ok(()) }).await.is_err());
    assert!(!dropped.load(Ordering::SeqCst));
    resume.send(()).unwrap();
    request.await.unwrap().unwrap();
    let (a, b) = tokio::join!(owner.shutdown(), owner.shutdown());
    a.unwrap();
    b.unwrap();
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn last_owner_drop_drains_accepted_job_and_releases_resource() {
    let (owner, dropped) = owner().await;
    let (entered, started) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    let request = tokio::spawn({
        let owner = owner.clone();
        async move {
            owner
                .execute(move |_| async move {
                    entered.send(()).unwrap();
                    resumed.await?;
                    Ok(())
                })
                .await
        }
    });
    started.await.unwrap();
    request.abort();
    drop(request.await);
    drop(owner);
    assert!(!dropped.load(Ordering::SeqCst));
    resume.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn policy_enforces_foreign_keys_on_fresh_connection() {
    use super::driver::{ConnectionPolicy, PolicyDriver};
    let owner = DatabaseOwner::open(8, 1, (), || async {
        let driver = PolicyDriver::new(
            toasty::db::Connect::new("turso::memory:").await?,
            ConnectionPolicy::Turso,
        );
        Ok(toasty::Db::builder().max_pool_size(1).build(driver).await?)
    })
    .await
    .unwrap();
    owner.execute(|mut db| async move {
        toasty::sql::statement("CREATE TABLE parent (id TEXT PRIMARY KEY)").exec(&mut db).await?;
        toasty::sql::statement("CREATE TABLE child (id TEXT PRIMARY KEY, parent TEXT NOT NULL REFERENCES parent(id))").exec(&mut db).await?;
        assert!(toasty::sql::statement("INSERT INTO child VALUES ('child', 'missing')").exec(&mut db).await.is_err());
        Ok(())
    }).await.unwrap();
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn normalized_schema_roundtrip_and_identity_constraints() {
    use super::{
        driver::{ConnectionPolicy, PolicyDriver},
        models,
    };
    let owner = DatabaseOwner::open(8, 1, (), || async {
        let driver = PolicyDriver::new(
            toasty::db::Connect::new("turso::memory:").await?,
            ConnectionPolicy::Turso,
        );
        Ok(toasty::Db::builder()
            .models(models::storage_models())
            .max_pool_size(1)
            .build(driver)
            .await?)
    })
    .await
    .unwrap();
    owner.execute(|mut db| async move {
        super::schema::initialize(
            &mut db,
            super::schema::Backend::Turso,
            &[super::schema::Component::Userapp],
        ).await?;
        let mut tx = db.transaction().await?;
        models::Application::create().app_id("app").lifecycle_id("life").lifecycle_epoch(1)
            .lifecycle_state("active").metadata_revision(1).created_at_us(1).updated_at_us(1)
            .exec(&mut tx).await?;
        let app = models::Application::get_by_app_id(&mut tx, "app").await?;
        assert_eq!(app.lifecycle_id, "life");
        assert_eq!(app.metadata_revision, 1);
        models::ActiveOperations::create().app_id("app").lifecycle_id("life").exec(&mut tx).await?;
        tx.commit().await?;
        assert!(toasty::sql::statement("UPDATE userapp_active_operations SET dev_operation_id='missing' WHERE app_id='app'").exec(&mut db).await.is_err());
        assert!(toasty::sql::statement("UPDATE userapps SET metadata_revision=0 WHERE app_id='app'").exec(&mut db).await.is_err());
        assert!(toasty::sql::statement("INSERT INTO userapp_runtime_configs (app_id,lifecycle_id,scope,revision,saved_version,updated_at_us) VALUES ('app','life','prod',1,1,1)").exec(&mut db).await.is_err());
        // Structured credential version can be read only through its private model.
        models::RuntimeConfigVersion::create().app_id("app").lifecycle_id("life").scope("prod")
            .version(1).request_id("save1").expected_revision(0).payload_version(1)
            .pg_username("runtime").pg_password("test-only").created_at_us(1).exec(&mut db).await?;
        models::RuntimeConfig::create().app_id("app").lifecycle_id("life").scope("prod")
            .revision(1).saved_version(1).updated_at_us(1).exec(&mut db).await?;
        assert!(toasty::sql::statement("UPDATE userapp_runtime_configs SET applying_version=1 WHERE app_id='app'").exec(&mut db).await.is_err());
        Ok(())
    }).await.unwrap();
    owner.shutdown().await.unwrap();
}

async fn schema_owner() -> DatabaseOwner {
    use super::{
        driver::{ConnectionPolicy, PolicyDriver},
        models,
    };
    DatabaseOwner::open(8, 1, (), || async {
        let driver = PolicyDriver::new(
            toasty::db::Connect::new("turso::memory:").await?,
            ConnectionPolicy::Turso,
        );
        Ok(toasty::Db::builder()
            .models(models::storage_models())
            .max_pool_size(1)
            .build(driver)
            .await?)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn baseline_is_repeatable_and_rejects_checksum_future_and_catalog_drift() {
    use super::schema::{self as storage_schema, Backend, Component};
    for tamper in [
        "UPDATE rcoder_schema_migrations SET checksum='changed'",
        "UPDATE rcoder_schema_migrations SET version=2",
        "DROP INDEX userapp_operations_history",
        "DROP INDEX userapp_operations_unfinished",
        "DROP TABLE userapp_activity",
    ] {
        let owner = schema_owner().await;
        owner
            .execute(move |mut db| async move {
                storage_schema::initialize(&mut db, Backend::Turso, &[Component::Userapp]).await?;
                storage_schema::initialize(&mut db, Backend::Turso, &[Component::Userapp]).await?;
                toasty::sql::statement(tamper).exec(&mut db).await?;
                assert!(
                    storage_schema::initialize(&mut db, Backend::Turso, &[Component::Userapp])
                        .await
                        .is_err(),
                    "accepted {tamper}"
                );
                Ok(())
            })
            .await
            .unwrap();
        owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn old_unversioned_database_is_preserved_and_rejected() {
    use super::schema::{self as storage_schema, Backend, Component};
    let owner = schema_owner().await;
    owner
        .execute(|mut db| async move {
            toasty::sql::statement(
                "CREATE TABLE userapp_lifecycles (app_id TEXT PRIMARY KEY, record TEXT)",
            )
            .exec(&mut db)
            .await?;
            toasty::sql::statement("INSERT INTO userapp_lifecycles VALUES ('old','retained')")
                .exec(&mut db)
                .await?;
            assert!(
                storage_schema::initialize(&mut db, Backend::Turso, &[Component::Userapp])
                    .await
                    .is_err()
            );
            assert_eq!(
                toasty::sql::query("SELECT record FROM userapp_lifecycles")
                    .exec(&mut db)
                    .await?
                    .len(),
                1
            );
            assert!(
                toasty::sql::query("SELECT component FROM rcoder_schema_migrations")
                    .exec(&mut db)
                    .await
                    .is_err()
            );
            Ok(())
        })
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_initialization_joins_before_returning_error() {
    let dropped = Arc::new(AtomicBool::new(false));
    let result = DatabaseOwner::open(8, 1, Resource(dropped.clone()), || async {
        anyhow::bail!("injected initialization failure")
    })
    .await;
    assert!(result.is_err());
    assert!(dropped.load(Ordering::SeqCst));
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn old_database_bytes_are_never_opened_by_downgraded_engine() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("userapp.turso.db");
    let old = b"deliberately invalid old engine file; never open or modify";
    std::fs::write(&path, old).unwrap();
    assert!(
        crate::userapp_lifecycle::TursoUserAppStore::open_exclusive(&path)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), old);
    assert!(!path.with_extension("db.rcoder-format").exists());
}

#[tokio::test]
async fn task_panic_is_unknown_and_every_shutdown_reports_failure() {
    let (owner, dropped) = owner().await;
    let failed = owner
        .execute::<(), _, _>(|_| async { panic!("injected task panic") })
        .await;
    assert!(
        failed
            .unwrap_err()
            .downcast_ref::<super::owner::OutcomeUnknown>()
            .is_some()
    );
    assert!(
        owner.execute(|_| async { Ok(()) }).await.is_err(),
        "Panic must close admission before notifying the caller"
    );
    let (left, right) = tokio::join!(owner.shutdown(), owner.shutdown());
    assert!(left.is_err() && right.is_err());
    assert!(owner.shutdown().await.is_err());
    assert!(dropped.load(Ordering::SeqCst));
    assert!(owner.execute(|_| async { Ok(()) }).await.is_err());
}

#[tokio::test]
async fn worker_thread_panic_is_replayed_to_all_shutdown_waiters() {
    struct PanicOnDrop;
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("injected worker cleanup panic");
        }
    }
    let owner = DatabaseOwner::open(1, 1, PanicOnDrop, || async {
        Ok(toasty::Db::builder().connect("turso::memory:").await?)
    })
    .await
    .unwrap();
    for _ in 0..2 {
        assert!(
            tokio::time::timeout(Duration::from_secs(3), owner.shutdown())
                .await
                .unwrap()
                .is_err()
        );
    }
    let (a, b) = tokio::join!(owner.shutdown(), owner.shutdown());
    assert!(a.is_err() && b.is_err());
}

#[tokio::test]
async fn full_queue_rejects_without_waiting_for_the_running_transaction() {
    let owner = DatabaseOwner::open(1, 1, (), || async {
        Ok(toasty::Db::builder().connect("turso::memory:").await?)
    })
    .await
    .unwrap();
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let running = tokio::spawn({
        let owner = owner.clone();
        async move {
            owner
                .execute(|_| async move {
                    entered_tx.send(()).unwrap();
                    release_rx.await?;
                    Ok(())
                })
                .await
        }
    });
    entered_rx.await.unwrap();
    let queued = owner.execute(|_| async { Ok(()) });
    tokio::pin!(queued);
    assert!(futures::poll!(&mut queued).is_pending());
    let overflow = tokio::time::timeout(
        Duration::from_millis(200),
        owner.execute(|_| async { Ok(()) }),
    )
    .await
    .unwrap();
    assert!(
        overflow.is_err(),
        "Queue admission must fail while both running and queued slots are occupied"
    );
    release_tx.send(()).unwrap();
    running.await.unwrap().unwrap();
    queued.await.unwrap();
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn abandoned_transaction_rolls_back_before_connection_reuse() {
    let (owner, _) = owner().await;
    assert!(
        owner
            .execute::<(), _, _>(|mut db| async move {
                let mut tx = db.transaction().await?;
                Probe::create()
                    .id(10)
                    .revision(1)
                    .value("must rollback")
                    .exec(&mut tx)
                    .await?;
                anyhow::bail!("injected error before commit");
            })
            .await
            .is_err()
    );
    owner
        .execute(|mut db| async move {
            assert!(
                Probe::filter_by_id(10)
                    .first()
                    .exec(&mut db)
                    .await?
                    .is_none()
            );
            Probe::create()
                .id(10)
                .revision(1)
                .value("after rollback")
                .exec(&mut db)
                .await?;
            assert_eq!(Probe::get_by_id(&mut db, 10).await?.value, "after rollback");
            Ok(())
        })
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn toasty_turso_rows_affected_preserves_cas_and_conflict_semantics() {
    let (owner, _) = owner().await;
    owner
        .execute(|mut db| async move {
            toasty::sql::statement(
                "CREATE TABLE affected(id TEXT PRIMARY KEY, value INTEGER NOT NULL)",
            )
            .exec(&mut db)
            .await?;
            for (sql, expected) in [
                ("INSERT INTO affected VALUES('a',1)", 1),
                (
                    "INSERT INTO affected VALUES('a',2) ON CONFLICT DO NOTHING",
                    0,
                ),
                ("UPDATE affected SET value=2 WHERE id='a' AND value=1", 1),
                ("UPDATE affected SET value=3 WHERE id='a' AND value=1", 0),
                ("DELETE FROM affected WHERE id='a' AND value=2", 1),
                ("DELETE FROM affected WHERE id='a'", 0),
            ] {
                assert_eq!(
                    toasty::sql::statement(sql).exec(&mut db).await?,
                    expected,
                    "{sql}"
                );
            }
            let mut tx = db.transaction().await?;
            toasty::sql::statement("INSERT INTO affected VALUES('rollback',1)")
                .exec(&mut tx)
                .await?;
            tx.rollback().await?;
            assert!(
                toasty::sql::query("SELECT id FROM affected WHERE id='rollback'")
                    .exec(&mut db)
                    .await?
                    .is_empty()
            );
            let mut tx = db.transaction().await?;
            toasty::sql::statement("INSERT INTO affected VALUES('commit',1)")
                .exec(&mut tx)
                .await?;
            tx.commit().await?;
            assert_eq!(
                toasty::sql::query("SELECT id FROM affected WHERE id='commit'")
                    .exec(&mut db)
                    .await?
                    .len(),
                1
            );
            Ok(())
        })
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
}

/// A large terminal history must not be scanned to find the small unfinished
/// set. This checks the actual Turso planner and result, not an index-name list.
#[tokio::test]
async fn unfinished_recovery_scan_uses_partial_index_and_catalog_rejects_wrong_predicate() {
    let owner = schema_owner().await;
    owner.execute(|mut db| async move {
        super::schema::initialize(&mut db, super::schema::Backend::Turso, &[super::schema::Component::Userapp]).await?;
        for statement in [
            "INSERT INTO userapps(app_id,lifecycle_id,lifecycle_epoch,lifecycle_state,metadata_revision,created_at_us,updated_at_us) VALUES('scale','life',1,'active',1,1,1)",
            "INSERT INTO userapp_operations(operation_id,app_id,lifecycle_id,kind,scope,state,revision,request_fingerprint,step,payload_version,checkpoint_json,created_at_us,updated_at_us) VALUES('remaining','scale','life','start','prod','pending',1,'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb','admitted',1,'{}',10001,10001)",
        ] {
            toasty::sql::statement(statement).exec(&mut db).await?;
        }
        // Turso 0.7.2 does not support recursive CTEs. Keep all 10,000
        // historical rows, using bounded multi-row INSERT batches instead.
        for batch in 0..40 {
            let values = (1..=250).map(|offset| {
                let n = batch * 250 + offset;
                format!("('history{n}','scale','life','start','prod','succeeded',1,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','done',1,'{{}}',{n},{n},{n})")
            }).collect::<Vec<_>>().join(",");
            toasty::sql::statement(format!("INSERT INTO userapp_operations(operation_id,app_id,lifecycle_id,kind,scope,state,revision,request_fingerprint,step,payload_version,checkpoint_json,created_at_us,updated_at_us,terminal_at_us) VALUES {values}"))
                .exec(&mut db).await?;
        }
        let plan = toasty::sql::query("EXPLAIN QUERY PLAN SELECT * FROM userapp_operations WHERE operation_id > '' AND terminal_at_us IS NULL ORDER BY operation_id LIMIT 100")
            .exec(&mut db).await?;
        assert!(format!("{plan:?}").contains("userapp_operations_unfinished"), "{plan:?}");
        let rows = toasty::sql::query("SELECT operation_id FROM userapp_operations WHERE operation_id > '' AND terminal_at_us IS NULL ORDER BY operation_id LIMIT 100")
            .exec(&mut db).await?;
        assert_eq!(rows.len(), 1);
        assert!(format!("{rows:?}").contains("remaining"));
        toasty::sql::statement("DROP INDEX userapp_operations_unfinished").exec(&mut db).await?;
        toasty::sql::statement("CREATE INDEX userapp_operations_unfinished ON userapp_operations(operation_id) WHERE terminal_at_us IS NOT NULL").exec(&mut db).await?;
        assert!(super::schema::initialize(&mut db, super::schema::Backend::Turso, &[super::schema::Component::Userapp]).await.is_err());
        Ok(())
    }).await.unwrap();
    owner.shutdown().await.unwrap();
}
