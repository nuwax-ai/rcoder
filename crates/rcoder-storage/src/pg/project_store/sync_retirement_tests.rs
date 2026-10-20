//! Real PG MVCC plus original durable-delete admission; no background sync.
use super::*;
use std::{sync::atomic::Ordering, task::Poll};

#[tokio::test]
async fn sync_snapshot_does_not_resurrect_locally_accepted_project_retirement() {
    let Some(dsn) = test_dsn().await else { return };
    let project_id = format!("sync-local-retirement-{}", uuid_suffix());
    let session_id = format!("session-{project_id}");
    let (store, _cleanup) = PgStore::connect(
        &PostgresConfig {
            max_connections: Some(4),
            min_connections: Some(1),
            ..pg_config(&dsn)
        },
        "test-ns".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap();
    let store = Arc::new(store);
    store
        .insert_with_session_durable(
            project_id.clone(),
            info_for(&project_id, Some(container_for(&project_id))),
            &session_id,
        )
        .await
        .unwrap();
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let generation = store
        .get(&project_id)
        .unwrap()
        .persistence_identity()
        .generation
        .clone();

    let (drained, after_drain) = tokio::sync::oneshot::channel();
    let (resume, waiting) = tokio::sync::oneshot::channel();
    let syncing_store = Arc::clone(&store);
    let syncing = tokio::spawn(async move {
        sync::sync_once_paused_after_drain(
            &syncing_store,
            syncing_store.inner(),
            &syncing_store.database,
            drained,
            waiting,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), after_drain)
        .await
        .unwrap()
        .unwrap();

    // A row lock blocks only this project's DELETE. Plain repeatable-read
    // SELECTs still see its committed project/session rows while deletion waits.
    let database = store.database.clone();
    let locked_id = project_id.clone();
    let (locked, lock_ready) = tokio::sync::oneshot::channel();
    let (unlock, unlock_requested) = tokio::sync::oneshot::channel();
    let holder = tokio::spawn(async move {
        database
            .execute(move |mut db| async move {
                let mut tx = db.transaction().await?;
                let rows = toasty::sql::query(
                    "SELECT project_id FROM projects WHERE project_id=$1 FOR UPDATE",
                )
                .bind(&locked_id)
                .exec(&mut tx)
                .await?;
                anyhow::ensure!(rows.len() == 1, "Owned project row is absent before delete");
                locked
                    .send(())
                    .map_err(|_| anyhow::anyhow!("Row-lock observer dropped"))?;
                let _ = unlock_requested.await;
                tx.rollback().await?;
                Ok(())
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), lock_ready)
        .await
        .unwrap()
        .unwrap();

    // Poll the actual public entry once: its synchronous admission removes the
    // mirror, records the retired generation, and registers immutable SQL work.
    // No test mutates a lifecycle marker or counter to manufacture this state.
    let removal = store.remove_durable(&project_id);
    tokio::pin!(removal);
    assert!(matches!(futures::poll!(removal.as_mut()), Poll::Pending));
    assert!(store.get(&project_id).is_none());
    assert!(store.get_by_session_id(&session_id).is_none());
    assert!(store.pending_ops.load(Ordering::Acquire) > 0);

    resume.send(()).unwrap();
    let synced = tokio::time::timeout(Duration::from_secs(5), syncing).await;
    let absent_during_delete =
        store.get(&project_id).is_none() && store.get_by_session_id(&session_id).is_none();

    // Always release the physical SQL lock before checking the regression, so
    // the failing baseline also drains its accepted durable operation.
    unlock.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), holder)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let removed = tokio::time::timeout(Duration::from_secs(5), removal)
        .await
        .unwrap();
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let id = project_id.clone();
    let sid = session_id.clone();
    let sql_retired = super::super::database::read(&store.database, move |tx| {
        Box::pin(async move {
            Ok(!super::super::repo::project_exists(tx, &id).await?
                && !super::super::repo::session_exists(tx, &sid).await?
                && super::super::repo::project_generation_is_retired(tx, &id, &generation).await?)
        })
    })
    .await
    .unwrap();
    let absent_after_delete =
        store.get(&project_id).is_none() && store.get_by_session_id(&session_id).is_none();
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);

    synced.unwrap().unwrap().unwrap();
    assert!(
        removed.is_some(),
        "The original durable delete must complete"
    );
    assert!(
        sql_retired,
        "Real PG must retain retirement and remove project/session"
    );
    assert!(
        absent_during_delete,
        "The old PG snapshot resurrected an already-admitted project/session before DELETE committed"
    );
    assert!(
        absent_after_delete,
        "DELETE completion cannot leave a resurrected local project/session"
    );
}
