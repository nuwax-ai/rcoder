//! Real PostgreSQL contracts. The strict E2E runner supplies an isolated PostgreSQL 17 DSN.
use super::{
    persist_ops::{PersistOp, structural_ops_for_insert},
    writer::{execute_op, lock_ops},
};
use shared_types::{ProjectAndContainerInfo, ProjectStore, ServiceType};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{sync::Arc, time::Duration};

async fn pool() -> Option<PgPool> {
    let dsn = std::env::var("RCODER_PG_TEST_DSN")
        .ok()
        .filter(|v| !v.is_empty());
    if dsn.is_none() {
        assert_ne!(
            std::env::var("RCODER_PG_TEST_STRICT").as_deref(),
            Ok("1"),
            "strict PG contracts require RCODER_PG_TEST_DSN"
        );
        return None;
    }
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&dsn.unwrap())
        .await
        .expect("isolated PostgreSQL");
    let version: String = sqlx::query_scalar("SHOW server_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        version.starts_with("17."),
        "contract environment must use PostgreSQL 17: {version}"
    );
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("all migrations");
    Some(pool)
}
fn info(id: &str, sid: &str) -> ProjectAndContainerInfo {
    let mut p = ProjectAndContainerInfo::new(id.into());
    p.set_service_type(Some(ServiceType::WebAgentRunner));
    p.add_session(sid);
    p
}
async fn commit(
    pool: &PgPool,
    ops: &[PersistOp],
) -> Vec<shared_types::persistence::PersistenceOperationOutcome> {
    let mut tx = pool.begin().await.unwrap();
    lock_ops(&mut tx, ops).await.unwrap();
    let mut outcomes = Vec::new();
    for op in ops {
        outcomes.push(execute_op(&mut tx, op).await.unwrap());
    }
    tx.commit().await.unwrap();
    outcomes
}
async fn generation(pool: &PgPool, table: &str, id: &str) -> Option<String> {
    match table {
        "projects" => sqlx::query_scalar("SELECT generation FROM projects WHERE project_id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap(),
        "sessions" => sqlx::query_scalar("SELECT generation FROM sessions WHERE session_id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .unwrap(),
        _ => panic!("unsupported test table"),
    }
}

#[tokio::test]
async fn lifecycle_contract_old_remove_preserves_replacement_and_no_resurrection() {
    let Some(pool) = pool().await else { return };
    let id = format!("generation-{}", crate::pg::test_support::uuid_suffix());
    let old = info(&id, &format!("{id}-old"));
    let old_ops = structural_ops_for_insert(&old, &format!("{id}-old")).unwrap();
    commit(&pool, &old_ops).await;
    let mut new = info(&id, &format!("{id}-new"));
    let mut identity = new.persistence_identity().clone();
    identity.predecessor = Some(old.persistence_identity().generation.clone());
    new.set_persistence_identity(identity);
    commit(
        &pool,
        &structural_ops_for_insert(&new, &format!("{id}-new")).unwrap(),
    )
    .await;
    let delayed = PersistOp::RemoveProject {
        project_id: id.clone(),
        generation: old.persistence_identity().generation.clone(),
    };
    assert_eq!(
        commit(&pool, &[delayed]).await,
        vec![shared_types::persistence::PersistenceOperationOutcome::Superseded]
    );
    assert_eq!(
        generation(&pool, "projects", &id).await.as_deref(),
        Some(new.persistence_identity().generation.as_str())
    );
    assert!(
        generation(&pool, "sessions", &format!("{id}-new"))
            .await
            .is_some()
    );
    commit(
        &pool,
        &[PersistOp::RemoveProject {
            project_id: id.clone(),
            generation: new.persistence_identity().generation.clone(),
        }],
    )
    .await;
    assert!(
        commit(&pool, &old_ops)
            .await
            .iter()
            .all(|o| *o == shared_types::persistence::PersistenceOperationOutcome::Superseded)
    );
    assert!(
        generation(&pool, "projects", &id).await.is_none(),
        "retired old generation must not resurrect after current deletion"
    );
    assert!(
        generation(&pool, "sessions", &format!("{id}-old"))
            .await
            .is_none()
    );
}

#[tokio::test]
async fn lifecycle_contract_delayed_clear_and_remove_preserve_reused_session() {
    let Some(pool) = pool().await else { return };
    let id = format!("sessions-{}", crate::pg::test_support::uuid_suffix());
    let sid = format!("{id}-same");
    let mut p = info(&id, &sid);
    commit(&pool, &structural_ops_for_insert(&p, &sid).unwrap()).await;
    let old = p.persistence_identity().sessions[&sid].clone();
    let clear = PersistOp::ClearSessions {
        project_id: id.clone(),
        generation: p.persistence_identity().generation.clone(),
        sessions: vec![(sid.clone(), old.clone())],
    };
    p.remove_session(&sid);
    p.add_session(&sid);
    let new = p.persistence_identity().sessions[&sid].clone();
    commit(&pool, &structural_ops_for_insert(&p, &sid).unwrap()).await;
    commit(
        &pool,
        &[
            clear,
            PersistOp::RemoveSession {
                session_id: sid.clone(),
                generation: old.clone(),
            },
        ],
    )
    .await;
    assert_eq!(generation(&pool, "sessions", &sid).await, Some(new));
    commit(
        &pool,
        &[PersistOp::AddSession {
            project_id: id.clone(),
            session_id: sid.clone(),
            project_generation: p.persistence_identity().generation.clone(),
            generation: old,
            predecessor: None,
            container_name: None,
        }],
    )
    .await;
    assert_ne!(generation(&pool, "sessions", &sid).await, None);
    commit(
        &pool,
        &[PersistOp::RemoveProject {
            project_id: id,
            generation: p.persistence_identity().generation.clone(),
        }],
    )
    .await;
}

#[tokio::test]
async fn lifecycle_contract_reload_and_cross_replica_sync_preserve_identity() {
    let Some(pool) = pool().await else { return };
    let dsn = std::env::var("RCODER_PG_TEST_DSN").unwrap();
    let config = crate::config::PostgresConfig {
        url: Some(dsn),
        ..Default::default()
    };
    let (a, _) = crate::pg::PgStore::connect(&config, "contract".into(), "cluster.local".into())
        .await
        .unwrap();
    let id = format!("reload-{}", crate::pg::test_support::uuid_suffix());
    let sid = format!("{id}-sid");
    a.insert_with_session_durable(id.clone(), Arc::new(info(&id, &sid)), &sid)
        .await
        .unwrap();
    let original = a.get(&id).unwrap();
    let (b, _) = crate::pg::PgStore::connect(&config, "contract".into(), "cluster.local".into())
        .await
        .unwrap();
    let loaded = b.get(&id).unwrap();
    assert_eq!(
        loaded.persistence_identity().generation,
        original.persistence_identity().generation
    );
    assert_eq!(
        loaded.persistence_identity().sessions[&sid],
        original.persistence_identity().sessions[&sid]
    );
    super::sync::sync_once(&b, b.inner(), &pool).await.unwrap();
    assert_eq!(
        b.get(&id).unwrap().persistence_identity().sessions[&sid],
        original.persistence_identity().sessions[&sid]
    );
    a.remove_durable(&id).await;
    assert!(a.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(b.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn lifecycle_contract_container_delete_preserves_changed_association() {
    let Some(pool) = pool().await else { return };
    let id = format!("container-fence-{}", crate::pg::test_support::uuid_suffix());
    let sid = format!("{id}-sid");
    let mut p = info(&id, &sid);
    let mut container = shared_types::ContainerBasicInfo {
        container_id: format!("{id}-old"),
        container_name: format!("{id}-name"),
        container_ip: "10.0.0.1".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: id.clone(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: "http://old".into(),
    };
    p.set_container(Some(container.clone()));
    commit(&pool, &structural_ops_for_insert(&p, &sid).unwrap()).await;
    let delayed = PersistOp::DeleteContainerWithProjects {
        container_id: container.container_id.clone(),
        projects: vec![(id.clone(), p.persistence_identity().generation.clone())],
    };
    container.container_id = format!("{id}-new");
    container.container_ip = "10.0.0.2".into();
    container.created_at = chrono::Utc::now();
    p.set_container(Some(container.clone()));
    commit(&pool, &structural_ops_for_insert(&p, &sid).unwrap()).await;
    let owned_delete = PersistOp::RemoveProjectForContainer {
        project_id: id.clone(),
        generation: p.persistence_identity().generation.clone(),
        container_id: format!("{id}-old"),
        container_name: container.container_name.clone(),
    };
    assert_eq!(
        commit(&pool, &[owned_delete]).await,
        vec![shared_types::persistence::PersistenceOperationOutcome::Superseded]
    );
    assert_eq!(
        commit(&pool, &[delayed]).await,
        vec![shared_types::persistence::PersistenceOperationOutcome::Superseded]
    );
    assert_eq!(
        generation(&pool, "projects", &id).await,
        Some(p.persistence_identity().generation.clone())
    );
    let cid: String =
        sqlx::query_scalar("SELECT container_id FROM containers WHERE container_name=$1")
            .bind(&container.container_name)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cid, container.container_id);
    commit(
        &pool,
        &[PersistOp::DeleteContainerWithProjects {
            container_id: container.container_id,
            projects: vec![(id, p.persistence_identity().generation.clone())],
        }],
    )
    .await;
}

#[tokio::test]
async fn lifecycle_contract_legacy_schema_backfill_is_stable() {
    let Some(pool) = pool().await else { return };
    let mut tx = pool.begin().await.unwrap();
    // A transaction-local private schema is rolled back even if an assertion fails.
    let schema = format!("backfill_{}", crate::pg::test_support::uuid_suffix());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT set_config('search_path',$1,true)")
        .bind(&schema)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0001_init.sql"))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO projects(project_id,service_type) VALUES('legacy','web-agent-runner')",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sessions(session_id,project_id) VALUES('legacy-session','legacy')")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../migrations/0004_lifecycle_identity.sql"
    ))
    .execute(&mut *tx)
    .await
    .unwrap();
    let project: String =
        sqlx::query_scalar("SELECT generation FROM projects WHERE project_id='legacy'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    let session_project: String = sqlx::query_scalar(
        "SELECT project_generation FROM sessions WHERE session_id='legacy-session'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(project, session_project);
    assert!(!project.is_empty());
    let repeated: String =
        sqlx::query_scalar("SELECT generation FROM projects WHERE project_id='legacy'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(project, repeated);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn lifecycle_contract_flush_failure_shared_between_concurrent_callers() {
    use std::sync::atomic::AtomicI64;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    pool.close().await;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tx.send(PersistOp::RemoveProject {
        project_id: "flush-contract".into(),
        generation: "old".into(),
    })
    .unwrap();
    let writer = super::writer::PersistWriter::spawn(pool, rx, Arc::new(AtomicI64::new(1)));
    let (a, b) = tokio::join!(
        writer.flush_outcome(Duration::from_secs(2)),
        writer.flush_outcome(Duration::from_secs(2))
    );
    assert_eq!(a, b);
    assert!(matches!(
        a,
        shared_types::FlushOutcome::Incomplete { pending: 1, .. }
    ));
    assert_eq!(writer.flush_outcome(Duration::from_secs(2)).await, a);
}

#[tokio::test]
async fn lifecycle_contract_cancelled_durable_write_is_queued_and_shutdown_waits() {
    let Some(pool) = pool().await else { return };
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_PG_TEST_DSN").unwrap()),
        ..Default::default()
    };
    let (store, _) =
        crate::pg::PgStore::connect(&config, "contract".into(), "cluster.local".into())
            .await
            .unwrap();
    let store = Arc::new(store);
    let id = format!("cancel-write-{}", crate::pg::test_support::uuid_suffix());
    let sid = format!("{id}-sid");
    let mut barrier = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,719324))")
        .bind(format!("project:{id}"))
        .execute(&mut *barrier)
        .await
        .unwrap();
    let worker = {
        let store = store.clone();
        let id = id.clone();
        let sid = sid.clone();
        tokio::spawn(async move {
            store
                .insert_with_session_durable(id.clone(), Arc::new(info(&id, &sid)), &sid)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while store
            .active_writes
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("write registered before database wait");
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(
        store.pending_ops.load(std::sync::atomic::Ordering::Acquire) > 0,
        "cancellation must preserve pending operations"
    );
    assert_eq!(store.writer().drain_count(), 0);
    let timed = store
        .shutdown_flush_outcome(Duration::from_millis(10))
        .await;
    assert!(matches!(timed, shared_types::FlushOutcome::TimedOut { .. }));
    assert!(
        store
            .insert_with_session_durable(
                "rejected-after-shutdown".into(),
                Arc::new(info("rejected-after-shutdown", "s")),
                "s"
            )
            .await
            .is_err()
    );
    barrier.rollback().await.unwrap();
    assert!(
        store
            .shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
    assert!(
        store
            .shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete(),
        "repeat successful shutdown retains success"
    );
    assert_eq!(
        store.writer().drain_count(),
        1,
        "caller timeout and repeated shutdown must share exactly one drain"
    );
    assert!(generation(&pool, "projects", &id).await.is_some());
    assert!(generation(&pool, "sessions", &sid).await.is_some());
    // The store is closed, so perform explicit scoped test cleanup by captured identity.
    let current = store.get(&id).unwrap();
    commit(
        &pool,
        &[PersistOp::RemoveProject {
            project_id: id,
            generation: current.persistence_identity().generation.clone(),
        }],
    )
    .await;
}
