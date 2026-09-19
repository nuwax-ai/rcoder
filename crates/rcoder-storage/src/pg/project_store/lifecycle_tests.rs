//! Real PostgreSQL contracts. The strict E2E runner supplies an isolated PostgreSQL 17 DSN.
use super::{
    persist_ops::{PersistOp, structural_ops_for_insert},
    writer::{execute_op, lock_ops},
};
use crate::db::owner::DatabaseOwner;
use shared_types::{ProjectAndContainerInfo, ProjectStore, ServiceType};
use std::{sync::Arc, time::Duration};

async fn pool() -> Option<DatabaseOwner> {
    let dsn = crate::pg::test_support::test_dsn().await?;
    Some(crate::pg::test_support::database(&dsn).await)
}
fn info(id: &str, sid: &str) -> ProjectAndContainerInfo {
    let mut p = ProjectAndContainerInfo::new(id.into());
    p.set_service_type(Some(ServiceType::WebAgentRunner));
    p.add_session(sid);
    p
}
fn snapshot_ops(p: &ProjectAndContainerInfo, sid: &str) -> anyhow::Result<Vec<PersistOp>> {
    let mut p = p.clone();
    let mut identity = p.persistence_identity().clone();
    if identity.revision == 0 {
        identity.revision = 1;
    }
    p.set_persistence_identity(identity);
    structural_ops_for_insert(&p, sid)
}
async fn commit(
    owner: &DatabaseOwner,
    ops: &[PersistOp],
) -> Vec<shared_types::persistence::PersistenceOperationOutcome> {
    let ops = ops.to_vec();
    owner
        .execute(move |mut db| async move {
            let mut tx = db.transaction().await?;
            lock_ops(&mut tx, &ops).await?;
            let mut outcomes = Vec::new();
            for op in &ops {
                outcomes.push(execute_op(&mut tx, op).await?);
            }
            tx.commit().await?;
            Ok(outcomes)
        })
        .await
        .unwrap()
}
async fn generation(pool: &DatabaseOwner, table: &str, id: &str) -> Option<String> {
    let sql = match table {
        "projects" => "SELECT generation FROM projects WHERE project_id=$1",
        "sessions" => "SELECT generation FROM sessions WHERE session_id=$1",
        _ => panic!("unsupported test table"),
    };
    crate::pg::test_support::optional_text(pool, sql, id).await
}

#[tokio::test]
async fn lifecycle_contract_old_remove_preserves_replacement_and_no_resurrection() {
    let Some(pool) = pool().await else { return };
    let id = format!("generation-{}", crate::pg::test_support::uuid_suffix());
    let old = info(&id, &format!("{id}-old"));
    let old_ops = snapshot_ops(&old, &format!("{id}-old")).unwrap();
    commit(&pool, &old_ops).await;
    let mut new = info(&id, &format!("{id}-new"));
    let mut identity = new.persistence_identity().clone();
    identity.predecessor = Some(old.persistence_identity().generation.clone());
    new.set_persistence_identity(identity);
    commit(&pool, &snapshot_ops(&new, &format!("{id}-new")).unwrap()).await;
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
            .all(|o| *o == shared_types::persistence::PersistenceOperationOutcome::Committed)
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
async fn lifecycle_contract_registration_receipt_replays_without_reapplying() {
    use shared_types::persistence::PersistenceOperationOutcome as Outcome;
    let Some(pool) = pool().await else { return };
    let id = format!("receipt{}", crate::pg::test_support::uuid_suffix());
    let sid = format!("{id}session");
    let project = info(&id, &sid);
    let operations = snapshot_ops(&project, &sid).unwrap();
    assert_eq!(commit(&pool, &operations).await, vec![Outcome::Committed]);
    // Model a lost COMMIT response: replay the exact queue command, not a newly
    // admitted write. Revision must not advance again.
    assert_eq!(commit(&pool, &operations).await, vec![Outcome::Committed]);
    assert_eq!(
        crate::pg::test_support::optional_text(
            &pool,
            "SELECT row_revision::text FROM projects WHERE project_id=$1",
            &id
        )
        .await
        .as_deref(),
        Some("1")
    );
    let mut changed = operations.clone();
    if let PersistOp::RegisterProject { project, .. } = &mut changed[0] {
        project.request_id = Some("different-input".into());
    }
    let rejected = pool
        .execute(move |mut db| async move {
            let mut tx = db.transaction().await?;
            lock_ops(&mut tx, &changed).await?;
            let result = execute_op(&mut tx, &changed[0]).await;
            tx.rollback().await?;
            Ok(result.is_err())
        })
        .await
        .unwrap();
    assert!(
        rejected,
        "same request identity must not accept different inputs"
    );
    commit(
        &pool,
        &[PersistOp::RemoveProject {
            project_id: id.clone(),
            generation: project.persistence_identity().generation.clone(),
        }],
    )
    .await;
    assert_eq!(commit(&pool, &operations).await, vec![Outcome::Committed]);
    assert!(generation(&pool, "projects", &id).await.is_none());
    assert!(generation(&pool, "sessions", &sid).await.is_none());
}

#[tokio::test]
async fn lifecycle_contract_delayed_clear_and_remove_preserve_reused_session() {
    let Some(pool) = pool().await else { return };
    let id = format!("sessions-{}", crate::pg::test_support::uuid_suffix());
    let sid = format!("{id}-same");
    let mut p = info(&id, &sid);
    commit(&pool, &snapshot_ops(&p, &sid).unwrap()).await;
    let old = p.persistence_identity().sessions[&sid].clone();
    let clear = PersistOp::ClearSessions {
        project_id: id.clone(),
        generation: p.persistence_identity().generation.clone(),
        sessions: vec![(sid.clone(), old.clone())],
    };
    p.remove_session(&sid);
    p.add_session(&sid);
    let mut identity = p.persistence_identity().clone();
    identity.revision = 2;
    p.set_persistence_identity(identity);
    let new = p.persistence_identity().sessions[&sid].clone();
    commit(&pool, &snapshot_ops(&p, &sid).unwrap()).await;
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
async fn lifecycle_contract_registration_rejects_session_conflict_atomically() {
    let Some(pool) = pool().await else { return };
    let id = format!(
        "atomicregistration{}",
        crate::pg::test_support::uuid_suffix()
    );
    let sid = format!("{id}session");
    let owner = info(&id, &sid);
    assert_eq!(
        commit(&pool, &snapshot_ops(&owner, &sid).unwrap()).await,
        vec![shared_types::persistence::PersistenceOperationOutcome::Committed]
    );
    let contender_id = format!("{id}contender");
    let contender = info(&contender_id, &sid);
    assert_eq!(
        commit(&pool, &snapshot_ops(&contender, &sid).unwrap()).await,
        vec![shared_types::persistence::PersistenceOperationOutcome::Superseded]
    );
    assert!(
        generation(&pool, "projects", &contender_id).await.is_none(),
        "session ownership rejection must roll back the new project"
    );
    assert_eq!(
        generation(&pool, "sessions", &sid).await,
        Some(owner.persistence_identity().sessions[&sid].clone())
    );
    commit(
        &pool,
        &[PersistOp::RemoveProject {
            project_id: id,
            generation: owner.persistence_identity().generation.clone(),
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
    let mut identity = p.persistence_identity().clone();
    identity.container = Some(shared_types::persistence::ContainerPersistenceIdentity {
        generation: "registeredold".into(),
        revision: 1,
        physical_uid: Some(container.container_id.clone()),
        predecessor: None,
        predecessor_revision: None,
    });
    p.set_persistence_identity(identity);
    p.set_container(Some(container.clone()));
    commit(&pool, &snapshot_ops(&p, &sid).unwrap()).await;
    // A matching physical UID alone does not authorize deletion: a queued
    // command must also own the exact registration generation and name.
    for wrong_target in [
        (
            container.container_name.clone(),
            "stalegeneration".to_owned(),
        ),
        (
            format!("{}other", container.container_name),
            "registeredold".to_owned(),
        ),
    ] {
        let stale_bulk = PersistOp::DeleteContainerWithProjects {
            container_id: container.container_id.clone(),
            containers: vec![wrong_target.clone()],
            projects: vec![(id.clone(), p.persistence_identity().generation.clone())],
        };
        let stale_project = PersistOp::RemoveProjectForContainer {
            project_id: id.clone(),
            generation: p.persistence_identity().generation.clone(),
            container_id: container.container_id.clone(),
            container_name: wrong_target.0,
            container_generation: wrong_target.1,
        };
        assert_eq!(
            commit(&pool, &[stale_bulk, stale_project]).await,
            vec![shared_types::persistence::PersistenceOperationOutcome::Superseded; 2]
        );
        assert_eq!(
            generation(&pool, "projects", &id).await,
            Some(p.persistence_identity().generation.clone()),
            "stale deletion must preserve the project and its container association"
        );
        assert_eq!(
            crate::pg::test_support::optional_text(
                &pool,
                "SELECT container_generation FROM containers WHERE container_name=$1",
                &container.container_name,
            )
            .await
            .as_deref(),
            Some("registeredold")
        );
        assert!(
            crate::pg::test_support::optional_text(
                &pool,
                "SELECT container_generation FROM container_tombstones WHERE container_name=$1",
                &container.container_name,
            )
            .await
            .is_none(),
            "rejected deletion must not retire the live registration"
        );
    }
    // A second replica loads the predecessor before the first replica replaces it.
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_PG_TEST_DSN").unwrap()),
        ..Default::default()
    };
    let (peer, _) = crate::pg::PgStore::connect(&config, "contract".into(), "cluster.local".into())
        .await
        .unwrap();
    assert_eq!(
        peer.container_registration
            .lock()
            .unwrap()
            .get(&container.container_name)
            .unwrap()
            .generation,
        "registeredold"
    );
    let delayed = PersistOp::DeleteContainerWithProjects {
        container_id: container.container_id.clone(),
        containers: vec![(container.container_name.clone(), "registeredold".into())],
        projects: vec![(id.clone(), p.persistence_identity().generation.clone())],
    };
    // Another writer advances the predecessor while this replica still holds
    // revision 1. A replacement captured from that stale view must not retire it.
    let original_ops = snapshot_ops(&p, &sid).unwrap();
    let mut updated_container = original_ops
        .iter()
        .find_map(|op| match op {
            PersistOp::RegisterProject {
                container: Some(snapshot),
                ..
            } => Some(snapshot.clone()),
            _ => None,
        })
        .unwrap();
    let mut stale_registration = original_ops.clone();
    if let PersistOp::RegisterProject {
        request_id,
        container: Some(c),
        project,
    } = &mut stale_registration[0]
    {
        *request_id = uuid::Uuid::new_v4().to_string();
        c.expected_revision = 1;
        c.service_url = "http://must-rollback".into();
        project.expected_revision = 99;
    } else {
        panic!("expected complete registration");
    }
    assert_eq!(
        commit(&pool, &stale_registration).await,
        vec![shared_types::persistence::PersistenceOperationOutcome::Superseded]
    );
    assert_eq!(
        crate::pg::test_support::optional_text(
            &pool,
            "SELECT service_url FROM containers WHERE container_name=$1",
            &container.container_name
        )
        .await
        .as_deref(),
        Some("http://old"),
        "project CAS rejection must roll back the preceding container update"
    );
    updated_container.expected_revision = 1;
    updated_container.service_url = "http://updated-predecessor".into();
    assert_eq!(
        commit(&pool, &[PersistOp::UpsertContainer(updated_container)]).await,
        vec![shared_types::persistence::PersistenceOperationOutcome::Committed]
    );
    container.container_id = format!("{id}-new");
    container.container_ip = "10.0.0.2".into();
    container.created_at = chrono::Utc::now();
    let mut identity = p.persistence_identity().clone();
    identity.revision = 2;
    identity.container = Some(shared_types::persistence::ContainerPersistenceIdentity {
        generation: "registerednew".into(),
        revision: 1,
        physical_uid: Some(container.container_id.clone()),
        predecessor: Some("registeredold".into()),
        predecessor_revision: Some(1),
    });
    p.set_persistence_identity(identity);
    p.set_container(Some(container.clone()));
    let stale_replacement = snapshot_ops(&p, &sid)
        .unwrap()
        .into_iter()
        .find_map(|op| match op {
            PersistOp::RegisterProject {
                container: Some(snapshot),
                ..
            } => Some(PersistOp::UpsertContainer(snapshot)),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        commit(&pool, &[stale_replacement]).await,
        vec![shared_types::persistence::PersistenceOperationOutcome::Superseded]
    );
    assert_eq!(
        crate::pg::test_support::optional_text(
            &pool,
            "SELECT service_url FROM containers WHERE container_name=$1",
            &container.container_name
        )
        .await
        .as_deref(),
        Some("http://updated-predecessor")
    );
    assert!(
        crate::pg::test_support::optional_text(
            &pool,
            "SELECT container_generation FROM container_tombstones WHERE container_name=$1",
            &container.container_name
        )
        .await
        .is_none()
    );
    let mut identity = p.persistence_identity().clone();
    identity.container.as_mut().unwrap().predecessor_revision = Some(2);
    p.set_persistence_identity(identity);
    commit(&pool, &snapshot_ops(&p, &sid).unwrap()).await;
    super::sync::sync_once(&peer, peer.inner(), &pool)
        .await
        .unwrap();
    let mirrored = peer.get(&id).unwrap();
    let registration = peer
        .container_registration
        .lock()
        .unwrap()
        .get(&container.container_name)
        .unwrap()
        .clone();
    assert_eq!(registration.generation, "registerednew");
    assert_eq!(
        registration.physical_uid.as_deref(),
        Some(container.container_id.as_str())
    );
    assert_eq!(
        mirrored.persistence_identity().container.as_ref(),
        Some(&registration),
        "peer hydration must publish the project and registration from the same generation"
    );
    assert_eq!(
        mirrored.container_info().unwrap().container_id,
        container.container_id
    );
    let owned_delete = PersistOp::RemoveProjectForContainer {
        project_id: id.clone(),
        generation: p.persistence_identity().generation.clone(),
        container_id: format!("{id}-old"),
        container_name: container.container_name.clone(),
        container_generation: "registeredold".into(),
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
    let cid = crate::pg::test_support::optional_text(
        &pool,
        "SELECT container_id FROM containers WHERE container_name=$1",
        &container.container_name,
    )
    .await
    .unwrap();
    assert_eq!(cid, container.container_id);
    commit(
        &pool,
        &[PersistOp::DeleteContainerWithProjects {
            container_id: container.container_id,
            containers: vec![(container.container_name.clone(), "registerednew".into())],
            projects: vec![(id, p.persistence_identity().generation.clone())],
        }],
    )
    .await;
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn lifecycle_contract_unversioned_schema_is_rejected_without_adoption() {
    let Some(dsn) = crate::pg::test_support::test_dsn().await else {
        return;
    };
    let owner = crate::pg::test_support::database(&dsn).await;
    let schema = format!("legacy_{}", uuid::Uuid::new_v4().simple());
    let namespace = schema.clone();
    owner
        .execute(move |mut db| async move {
            let mut tx = db.transaction().await?;
            toasty::sql::statement(format!("CREATE SCHEMA {namespace}"))
                .exec(&mut tx)
                .await?;
            toasty::sql::statement(format!(
                "CREATE TABLE {namespace}.projects(project_id TEXT PRIMARY KEY)"
            ))
            .exec(&mut tx)
            .await?;
            tx.commit().await?;
            Ok(())
        })
        .await
        .unwrap();
    let separator = if dsn.contains('?') { "&" } else { "?" };
    let url = format!("{dsn}{separator}options=-csearch_path%3D{schema}");
    let rejected = crate::db::postgres::open(
        &crate::config::PostgresConfig {
            url: Some(url),
            ..Default::default()
        },
        vec![crate::db::schema::Component::Project],
    )
    .await;
    let namespace = schema.clone();
    let columns = owner.execute(move |mut db| async move {
        let columns = toasty::sql::query("SELECT column_name::text FROM information_schema.columns WHERE table_schema=$1 AND table_name='projects'")
            .bind(&namespace).exec(&mut db).await?;
        toasty::sql::statement(format!("DROP SCHEMA {namespace} CASCADE")).exec(&mut db).await?;
        Ok(columns.len())
    }).await.unwrap();
    assert!(
        rejected.is_err(),
        "An old unversioned schema must not be silently adopted"
    );
    assert_eq!(
        columns, 1,
        "Rejected initialization must not add columns to the old table"
    );
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn lifecycle_contract_flush_failure_shared_between_concurrent_callers() {
    use std::sync::atomic::AtomicI64;
    let pool = DatabaseOwner::closed_for_test();
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
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let barrier_owner = pool.clone();
    let key = format!("project:{id}");
    let barrier = tokio::spawn(async move {
        barrier_owner
            .execute(move |mut db| async move {
                let mut tx = db.transaction().await?;
                toasty::sql::query(
                    "SELECT 1 FROM pg_advisory_xact_lock(hashtextextended($1,719324))",
                )
                .bind(key)
                .exec(&mut tx)
                .await?;
                let _ = locked_tx.send(());
                let _ = release_rx.await;
                tx.rollback().await?;
                Ok(())
            })
            .await
    });
    locked_rx.await.unwrap();
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
    release_tx.send(()).unwrap();
    barrier.await.unwrap().unwrap();
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
