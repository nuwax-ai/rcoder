//! Real commit-stage evidence for configurable budgets; every trigger is confined
//! to an owned test schema, and no background task fabricates a commit result.
use super::cross_replica_existing_session::{hold_store_connection, single_connection_peer};
use super::*;
use crate::db::owner::DatabaseOwner;

struct CommitFixture {
    store: PgStore,
    observer: DatabaseOwner,
    project_id: String,
    lock_key: String,
}

impl CommitFixture {
    async fn new(budget: Option<u64>) -> Option<Self> {
        let dsn = test_support::test_dsn().await?;
        let schema = format!("durable_commit_{}", uuid::Uuid::new_v4().simple());
        let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
        toasty::sql::statement(format!("CREATE SCHEMA {schema}"))
            .exec(&mut admin)
            .await
            .unwrap();
        drop(admin);
        let mut scoped = url::Url::parse(&dsn).unwrap();
        let params = scoped
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        let options = params
            .iter()
            .filter(|(key, _)| key == "options")
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        {
            let mut query = scoped.query_pairs_mut();
            query.clear();
            query.extend_pairs(
                params
                    .iter()
                    .filter(|(key, _)| key != "options")
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            );
            query.append_pair("options", &format!("{options} -csearch_path={schema}"));
        }
        let query = scoped.query().map(|query| query.replace('+', "%20"));
        scoped.set_query(query.as_deref());
        let dsn: String = scoped.into();
        let mut config = PostgresConfig {
            url: Some(dsn.clone()),
            ..Default::default()
        };
        if let Some(budget) = budget {
            config.durable_commit_timeout_ms = Some(budget);
        }
        let store = PgStore::connect(&config, "commit-budget-test".into(), "cluster.local".into())
            .await
            .unwrap();
        let observer = test_support::database(&dsn).await;
        let project_id = format!("commit-{}", uuid::Uuid::new_v4().simple());
        let lock_key = format!("commit-probe-{project_id}");
        let key = lock_key.clone();
        store.database.execute(move |mut db| async move {
            toasty::sql::statement("CREATE TABLE durable_commit_probe (id integer PRIMARY KEY, commits integer NOT NULL)").exec(&mut db).await?;
            // This is a deferred constraint trigger: INSERT has completed and
            // the caller is awaiting COMMIT when this exact lock is requested.
            toasty::sql::statement(format!("CREATE FUNCTION durable_commit_probe_fn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(hashtextextended('{key}',719324)); INSERT INTO durable_commit_probe VALUES (1,1) ON CONFLICT(id) DO UPDATE SET commits=durable_commit_probe.commits+1; RETURN NEW; END $$")).exec(&mut db).await?;
            toasty::sql::statement("CREATE CONSTRAINT TRIGGER durable_commit_probe_trigger AFTER INSERT OR UPDATE ON projects DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION durable_commit_probe_fn()").exec(&mut db).await?;
            Ok(())
        }).await.unwrap();
        Some(Self {
            store,
            observer,
            project_id,
            lock_key,
        })
    }

    fn info(&self) -> Arc<ProjectAndContainerInfo> {
        project(&self.project_id, &self.project_id, &basic(&self.project_id))
    }

    async fn hold_commit_lock(
        &self,
    ) -> (
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        // The observer was initialized against the unmodified product schema.
        // Reopening through test_support::database after fault-injection DDL
        // would correctly fail the strict catalog check. Reuse its pool instead;
        // a transaction holds one connection while observations use another.
        let owner = self.observer.clone();
        let key = self.lock_key.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            owner
                .execute(move |mut db| async move {
                    let mut tx = db.transaction().await?;
                    toasty::sql::query(
                        "SELECT 1 FROM pg_advisory_xact_lock(hashtextextended($1,719324))",
                    )
                    .bind(key)
                    .exec(&mut tx)
                    .await?;
                    started.send(()).unwrap();
                    let _ = wait.await;
                    tx.rollback().await?;
                    Ok(())
                })
                .await
                .unwrap();
        });
        ready.await.unwrap();
        (release, task)
    }

    async fn reached_commit(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count = test_support::optional_text(&self.observer, "SELECT count(*)::text FROM pg_locks WHERE locktype='advisory' AND NOT granted AND classid::bigint=((hashtextextended($1,719324)>>32)&4294967295) AND objid::bigint=(hashtextextended($1,719324)&4294967295) AND objsubid=1", &self.lock_key).await.unwrap();
                if count != "0" { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("the real deferred trigger must reach COMMIT");
        let visible = test_support::optional_text(
            &self.observer,
            "SELECT count(*)::text FROM projects WHERE project_id=$1",
            &self.project_id,
        )
        .await
        .unwrap();
        assert_eq!(
            visible, "0",
            "an uncommitted registration is not durable evidence"
        );
    }

    async fn assert_one_commit(&self) {
        assert!(self.store.wait_drained(Duration::from_secs(5)).await);
        assert_eq!(
            test_support::optional_text(
                &self.observer,
                "SELECT commits::text FROM durable_commit_probe WHERE id::text=$1",
                "1"
            )
            .await
            .as_deref(),
            Some("1")
        );
        assert_eq!(
            test_support::optional_text(
                &self.observer,
                "SELECT count(*)::text FROM project_write_receipts WHERE outcome=$1",
                "committed"
            )
            .await
            .as_deref(),
            Some("1")
        );
        let revision = test_support::optional_text(
            &self.observer,
            "SELECT row_revision::text FROM projects WHERE project_id=$1",
            &self.project_id,
        )
        .await;
        assert_eq!(revision.as_deref(), Some("1"));
    }

    async fn finish(self) {
        assert!(
            self.store
                .shutdown_flush_outcome(Duration::from_secs(5))
                .await
                .is_complete()
        );
        self.observer.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn invalid_durable_budget_is_rejected_before_database_connection() {
    for budget in [0, u64::MAX] {
        let config = PostgresConfig {
            url: Some("invalid-dsn-that-must-not-be-opened".into()),
            durable_commit_timeout_ms: Some(budget),
            ..Default::default()
        };
        let error = PgStore::connect(&config, "test".into(), "cluster.local".into())
            .await
            .err()
            .expect("invalid budget must fail before connect");
        assert!(
            error.to_string().contains("durable_commit_timeout_ms"),
            "{error:#}"
        );
    }
}

#[tokio::test]
async fn refresh_initial_sql_read_obeys_the_configured_total_deadline() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let store = single_connection_peer(&dsn).await;
    let id = format!("refresh-budget-{}", test_support::uuid_suffix());
    assert_eq!(
        store
            .insert_project_registration_durable(id.clone(), project(&id, &id, &basic(&id)))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let before = store.get(&id).unwrap();
    let (release, held) = hold_store_connection(&store).await;
    let attempt = tokio::time::timeout(
        Duration::from_secs(2),
        store.refresh_project_registration(&id),
    )
    .await;
    release.send(()).unwrap();
    held.await.unwrap();
    let error = attempt
        .expect("the configured 600ms budget must bound the initial read")
        .err()
        .expect("lookup must report its deadline");
    assert!(
        error
            .to_string()
            .contains("refresh exceeded the durable deadline")
    );
    assert!(Arc::ptr_eq(&before, &store.get(&id).unwrap()));
    assert!(
        store
            .shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
}

#[tokio::test]
async fn default_budget_waits_for_real_commit_beyond_the_old_600ms_budget() {
    let Some(f) = CommitFixture::new(None).await else {
        return;
    };
    assert_eq!(f.store.durable_commit_budget(), Duration::from_secs(15));
    for (setting, expected) in [("statement_timeout", "15s"), ("lock_timeout", "2s")] {
        assert_eq!(
            test_support::optional_text(&f.store.database, "SELECT current_setting($1)", setting)
                .await
                .as_deref(),
            Some(expected)
        );
    }
    let (release, held) = f.hold_commit_lock().await;
    let mut write = Box::pin(
        f.store
            .insert_project_registration_durable(f.project_id.clone(), f.info()),
    );
    assert!(futures::poll!(write.as_mut()).is_pending());
    f.reached_commit().await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        futures::poll!(write.as_mut()).is_pending(),
        "no Committed response before SQL COMMIT"
    );
    release.send(()).unwrap();
    held.await.unwrap();
    assert_eq!(write.await.unwrap(), PersistenceWriteOutcome::Committed);
    f.assert_one_commit().await;
    f.finish().await;
}

#[tokio::test]
async fn commit_timeout_reconciles_the_original_receipt_without_reapplying() {
    let Some(f) = CommitFixture::new(Some(300)).await else {
        return;
    };
    let (release, held) = f.hold_commit_lock().await;
    let mut write = Box::pin(
        f.store
            .insert_project_registration_durable(f.project_id.clone(), f.info()),
    );
    assert!(futures::poll!(write.as_mut()).is_pending());
    f.reached_commit().await;
    let outcome = tokio::time::timeout(Duration::from_secs(2), write)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, PersistenceWriteOutcome::Deferred { .. }));
    release.send(()).unwrap();
    held.await.unwrap();
    f.assert_one_commit().await;
    f.finish().await;
}

#[tokio::test]
async fn caller_cancellation_during_commit_keeps_the_same_registered_operation() {
    let Some(f) = CommitFixture::new(None).await else {
        return;
    };
    let (release, held) = f.hold_commit_lock().await;
    let mut write = Box::pin(
        f.store
            .insert_project_registration_durable(f.project_id.clone(), f.info()),
    );
    assert!(futures::poll!(write.as_mut()).is_pending());
    f.reached_commit().await;
    drop(write);
    release.send(()).unwrap();
    held.await.unwrap();
    f.assert_one_commit().await;
    f.finish().await;
}
