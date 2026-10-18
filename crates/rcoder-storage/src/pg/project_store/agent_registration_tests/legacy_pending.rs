//! Legacy placeholders remain evidence, never authoritative session routing.
use super::*;
use crate::pg::project_store::persist_ops::PersistOp;

async fn isolated() -> (
    tokio_postgres::Client,
    tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
    String,
    String,
) {
    let dsn = test_support::test_dsn()
        .await
        .expect("strict disposable PG");
    let (client, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    let task = tokio::spawn(connection);
    let schema = format!("pendinghint{}", uuid::Uuid::new_v4().simple());
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; SET search_path TO {schema}"
        ))
        .await
        .unwrap();
    let separator = if dsn.contains('?') { '&' } else { '?' };
    let scoped = format!("{dsn}{separator}options=-csearch_path%3D{schema}");
    (client, task, schema, scoped)
}

async fn legacy_row(client: &tokio_postgres::Client) -> String {
    client
        .query_one(
            "SELECT row_to_json(s)::text FROM sessions s WHERE session_id='pending'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

#[tokio::test]
async fn legacy_pending_sql_history_survives_metadata_real_completion_and_reload() {
    if test_support::test_dsn().await.is_none() {
        return;
    }
    let (client, task, schema, dsn) = isolated().await;
    let writer = store(&dsn).await;
    let id = "legacy-chat";
    assert_eq!(
        writer
            .insert_project_registration_durable(id.into(), project(id, "owner", &basic("owner")))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let generation = writer
        .get(id)
        .unwrap()
        .persistence_identity()
        .generation
        .clone();
    client.execute("INSERT INTO sessions(session_id,generation,project_id,project_generation,created_at_us,last_seen_at_us) VALUES('pending','legacy-session-generation',$1,$2,1,2)", &[&id,&generation]).await.unwrap();
    client
        .execute(
            "UPDATE projects SET latest_session='pending' WHERE project_id=$1",
            &[&id],
        )
        .await
        .unwrap();
    let raw = legacy_row(&client).await;
    let reader = store(&dsn).await;
    let original = reader.get(id).unwrap();
    assert_eq!(original.latest_session(), Some("pending"));
    assert_eq!(
        original.persistence_identity().sessions["pending"],
        "legacy-session-generation"
    );
    // Metadata must not refresh/rewrite the historical placeholder row.
    let mut metadata = (*original).clone();
    metadata.set_request_id(Some("new-metadata".into()));
    assert_eq!(
        reader
            .insert_project_registration_durable(id.into(), Arc::new(metadata))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(
        legacy_row(&client).await,
        raw,
        "legacy row is immutable evidence, not an active session touch"
    );
    assert!(reader.get_by_session_id("pending").is_none());
    assert!(reader.get_container_name_by_session("pending").is_none());
    assert!(
        reader
            .get_by_session_id_with_fetch_result("pending")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        crate::pg::project_store::database::read(&reader.database, |tx| {
            Box::pin(async move {
                crate::pg::project_store::repo::fetch_project_by_session(tx, "pending").await
            })
        })
        .await
        .unwrap()
        .is_none(),
        "direct SQL point reads must not publish legacy routes either"
    );
    let stale = store(&dsn).await;
    let expected = frozen(&reader.get(id).unwrap());
    assert_eq!(
        reader
            .append_session_for_container_durable(&expected, "pending-prefix/real-sid")
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let mut stale_metadata = (*stale.get(id).unwrap()).clone();
    stale_metadata.set_request_id(Some("stale-placeholder-metadata".into()));
    // Exercise a whole-project snapshot, not the metadata helper's existing
    // preserve_latest flag. Its obsolete placeholder cannot clear the real SID.
    stale.insert(id.into(), Arc::new(stale_metadata)).unwrap();
    assert!(
        stale
            .writer()
            .flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
    let latest: String = client
        .query_one(
            "SELECT latest_session FROM projects WHERE project_id=$1",
            &[&id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(latest, "pending-prefix/real-sid");
    assert_eq!(legacy_row(&client).await, raw);
    let reloaded = store(&dsn).await;
    let current = reloaded.get(id).unwrap();
    assert!(current.sessions().contains("pending"));
    assert_eq!(current.latest_session(), Some("pending-prefix/real-sid"));
    assert!(
        reloaded
            .get_by_session_id_with_fetch_result("pending")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        reloaded
            .get_by_session_id_with_fetch_result("pending-prefix/real-sid")
            .await
            .unwrap()
            .is_some()
    );
    for store in [&writer, &reader, &stale, &reloaded] {
        assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
    }
    drop((writer, reader, stale, reloaded));
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    drop(client);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn legacy_pending_replay_is_superseded_and_new_real_sessions_keep_foreign_guards() {
    if test_support::test_dsn().await.is_none() {
        return;
    }
    let (client, task, schema, dsn) = isolated().await;
    let writer = store(&dsn).await;
    let id = "new-chat";
    writer
        .insert_project_registration_durable(id.into(), project(id, "owner", &basic("owner")))
        .await
        .unwrap();
    let expected = frozen(&writer.get(id).unwrap());
    let replay = writer.register_durable(vec![PersistOp::AddSession {
        project_id: id.into(),
        session_id: "pending".into(),
        project_generation: expected.persistence_identity().generation.clone(),
        generation: "old-queued-generation".into(),
        predecessor: None,
    }]);
    assert_eq!(
        writer
            .execute_durable(replay, "legacy_pending_test_replay")
            .await,
        PersistenceWriteOutcome::Superseded
    );
    let count: i64 = client
        .query_one(
            "SELECT count(*) FROM sessions WHERE session_id='pending'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
    assert_eq!(
        writer
            .pending_ops
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert!(
        writer
            .append_session_for_container_durable(&expected, "pending")
            .await
            .is_err()
    );
    assert!(!writer.add_session_durable(id, "pending").await.unwrap());
    assert!(
        writer
            .insert_with_session_durable(
                "rejected".into(),
                project("rejected", "owner-2", &basic("owner-2")),
                "pending"
            )
            .await
            .is_err()
    );
    assert!(!writer.contains_key("rejected"));
    assert!(
        writer
            .insert_with_session(
                "rejected-sync".into(),
                project("rejected-sync", "owner-3", &basic("owner-3")),
                Some("pending")
            )
            .is_err()
    );
    assert!(!writer.contains_key("rejected-sync"));
    assert_eq!(
        writer
            .append_session_for_container_durable(&expected, "opaque/real-session")
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    writer
        .insert_project_registration_durable(
            "foreign".into(),
            project("foreign", "foreign-owner", &basic("foreign-owner")),
        )
        .await
        .unwrap();
    let foreign = frozen(&writer.get("foreign").unwrap());
    assert!(
        writer
            .append_session_for_container_durable(&foreign, "opaque/real-session")
            .await
            .is_err()
    );
    assert_eq!(
        writer
            .get_by_session_id_with_fetch_result("opaque/real-session")
            .await
            .unwrap()
            .unwrap()
            .project_id(),
        id
    );
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
    drop(writer);
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
    drop(client);
    task.await.unwrap().unwrap();
}
