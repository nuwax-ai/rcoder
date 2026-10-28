#![cfg(all(test, feature = "pg"))]
use super::{database, persist_ops::PersistOp, userapp_agent, writer};
use crate::{
    ProjectStoreBackend,
    config::PostgresConfig,
    db::schema::Component,
    pg::{PgStore, test_support},
};
use shared_types::{ProjectAndContainerInfo, ProjectStore, ServiceType};
use std::{sync::Arc, time::Duration};

fn deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(5)
}
async fn fixture() -> Option<(
    String,
    String,
    String,
    Arc<PgStore>,
    crate::db::owner::DatabaseOwner,
)> {
    let dsn = test_support::test_dsn().await?;
    let config = PostgresConfig {
        url: Some(dsn.clone()),
        ..Default::default()
    };
    let owner = crate::db::postgres::open(&config, vec![Component::Userapp, Component::Project])
        .await
        .expect("schema");
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let app_id = format!("binding-{suffix}");
    let lifecycle_id = format!("life-{suffix}");
    let (app, life) = (app_id.clone(), lifecycle_id.clone());
    owner.execute(move |mut db| async move {
        toasty::sql::statement("INSERT INTO userapps(app_id,lifecycle_id,lifecycle_epoch,lifecycle_state,metadata_revision,created_at_us,updated_at_us) VALUES($1,$2,1,'active',1,1,1)")
            .bind(app).bind(life).exec(&mut db).await?;
        Ok(())
    }).await.unwrap();
    let store = PgStore::connect(&config, "binding-test".into(), "cluster.local".into())
        .await
        .unwrap();
    let store = Arc::new(store);
    let mut info = ProjectAndContainerInfo::new(app_id.clone());
    info.set_service_type(Some(ServiceType::UserappBuilder));
    info.set_request_id(Some("builder-metadata".into()));
    info.set_container(Some(shared_types::ContainerBasicInfo {
        container_id: format!("pod-{suffix}"),
        container_name: format!("builder-{app_id}"),
        container_ip: "127.0.0.1".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: app_id.clone(),
        status: "Running".into(),
        service_url: "http://127.0.0.1:50051".into(),
        created_at: chrono::Utc::now(),
        workload_uid: Some(format!("sts-{suffix}")),
    }));
    store.insert(app_id.clone(), Arc::new(info)).unwrap();
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    Some((dsn, app_id, lifecycle_id, store, owner))
}

#[tokio::test]
async fn userapp_pending_direct_pg_registration_is_rejected_without_poisoning_next_session() {
    let Some((_dsn, app, life, store, owner)) = fixture().await else {
        return;
    };
    let backend = ProjectStoreBackend::Postgres(store.clone());
    let binding = backend
        .accept_userapp_agent_binding(&app, &life, &format!("chat-{app}"), "test-user")
        .await
        .unwrap();
    let generation = store
        .get(&app)
        .unwrap()
        .persistence_identity()
        .generation
        .clone();
    assert!(
        store
            .register_userapp_agent_session_for_route(
                &binding,
                "pending",
                &format!("invalid-{app}"),
                deadline(),
                &generation,
            )
            .await
            .is_err()
    );
    assert_eq!(
        backend
            .get_userapp_agent_binding(&app, &life, &binding.project_id)
            .await
            .unwrap(),
        Some(binding.clone())
    );
    assert!(
        store
            .get_userapp_agent_session("pending")
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.get(&app).unwrap().sessions().is_empty());
    let sid = format!("pending-prefix-{app}");
    let current = store
        .register_userapp_agent_session_for_route(
            &binding,
            &sid,
            &format!("valid-{app}"),
            deadline(),
            &generation,
        )
        .await
        .unwrap();
    assert_eq!(current.current_session_id, Some(sid.clone()));
    assert!(
        store
            .get_userapp_agent_session(&sid)
            .await
            .unwrap()
            .is_some()
    );
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn userapp_bindings_parallel_registration_and_cross_replica_read_keep_one_app_project() {
    let Some((dsn, app, life, store, _owner)) = fixture().await else {
        return;
    };
    let a = ProjectStoreBackend::Postgres(store.clone());
    let project_a = format!("a-{app}");
    let project_b = format!("b-{app}");
    let binding_a = a
        .accept_userapp_agent_binding(&app, &life, &project_a, "test-user")
        .await
        .unwrap();
    let binding_b = a
        .accept_userapp_agent_binding(&app, &life, &project_b, "test-user")
        .await
        .unwrap();
    let sid_a = format!("sid-{project_a}");
    let sid_b = format!("sid-{project_b}");
    let request_a = format!("request-{project_a}");
    let request_b = format!("request-{project_b}");
    let before = store.inner().get(&app).unwrap();
    let (ra, rb) = tokio::join!(
        a.register_userapp_agent_session(&binding_a, &sid_a, &request_a, deadline()),
        a.register_userapp_agent_session(&binding_b, &sid_b, &request_b, deadline())
    );
    assert_eq!(
        ra.unwrap().current_session_id.as_deref(),
        Some(sid_a.as_str())
    );
    assert_eq!(
        rb.unwrap().current_session_id.as_deref(),
        Some(sid_b.as_str())
    );
    let config = PostgresConfig {
        url: Some(dsn),
        ..Default::default()
    };
    let other = PgStore::connect(&config, "binding-test".into(), "cluster.local".into())
        .await
        .unwrap();
    let b = ProjectStoreBackend::Postgres(Arc::new(other));
    assert_eq!(
        b.get_userapp_agent_session(&sid_a)
            .await
            .unwrap()
            .unwrap()
            .0
            .project_id,
        project_a
    );
    assert_eq!(
        b.get_userapp_agent_session(&sid_b)
            .await
            .unwrap()
            .unwrap()
            .0
            .project_id,
        project_b
    );
    let physical = b.get_by_session_with_fetch(&sid_a).await.unwrap();
    assert_eq!(physical.project_id(), app);
    assert_eq!(physical.request_id(), before.request_id());
    assert!(physical.sessions().contains(&sid_a) && physical.sessions().contains(&sid_b));
    assert!(b.get(&project_a).is_none() && b.get(&project_b).is_none());
}

#[tokio::test]
async fn userapp_mapping_receipt_replay_does_not_reapply_old_binding_revision() {
    let Some((_dsn, app, life, store, _owner)) = fixture().await else {
        return;
    };
    let backend = ProjectStoreBackend::Postgres(store.clone());
    let project = format!("project-{app}");
    let owner = backend
        .accept_userapp_agent_binding(&app, &life, &project, "test-user")
        .await
        .unwrap();
    let app_generation = store
        .inner()
        .get(&app)
        .unwrap()
        .persistence_identity()
        .generation
        .clone();
    let sid = format!("sid-{app}");
    let session_generation = uuid::Uuid::new_v4().to_string();
    let command = crate::userapp_agent::MappingCommand::Register {
        binding: owner.clone(),
        session: shared_types::UserAppAgentSession {
            session_id: sid.clone(),
            session_generation: session_generation.clone(),
            binding_generation: owner.binding_generation.clone(),
            created_at: chrono::Utc::now(),
        },
        app_project_generation: app_generation,
    };
    let op = PersistOp::UserAppAgentMapping {
        request_id: format!("lost-reply-{app}"),
        command: Box::new(command),
    };
    assert_eq!(
        writer::execute_registered(&store.database, vec![op.clone()])
            .await
            .unwrap(),
        0
    );
    // This is the exact replay made after losing a successful COMMIT reply. Without
    // the receipt the captured old revision now rejects this same registration.
    assert_eq!(
        writer::execute_registered(&store.database, vec![op])
            .await
            .unwrap(),
        0
    );
    let found = backend
        .get_userapp_agent_session(&sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.0.revision, owner.revision + 1);
    assert_eq!(found.1.session_generation, session_generation);
}

#[tokio::test]
async fn userapp_same_captured_sid_is_confirmed_without_rewriting_superseded_receipt() {
    let Some((_dsn, app, life, store, db)) = fixture().await else {
        return;
    };
    let backend = ProjectStoreBackend::Postgres(store.clone());
    let project = format!("same-sid-{app}");
    let captured = backend
        .accept_userapp_agent_binding(&app, &life, &project, "test-user")
        .await
        .unwrap();
    let also_captured = backend
        .accept_userapp_agent_binding(&app, &life, &project, "test-user")
        .await
        .unwrap();
    assert_eq!(captured, also_captured);
    let app_generation = store
        .inner()
        .get(&app)
        .unwrap()
        .persistence_identity()
        .generation
        .clone();
    let sid = format!("same-sid-{app}");
    let first_request = format!("first-{app}");
    let second_request = format!("second-{app}");
    let (first, second) = tokio::join!(
        backend.register_userapp_agent_session_for_route(
            &captured,
            &sid,
            &first_request,
            deadline(),
            &app_generation
        ),
        backend.register_userapp_agent_session_for_route(
            &also_captured,
            &sid,
            &second_request,
            deadline(),
            &app_generation
        )
    );
    let first = first.expect("first concurrently accepted prompt mapping");
    let second = second.expect("same actual SID/route must confirm after losing the binding CAS");
    assert_eq!(first, second);
    assert_eq!(first.revision, captured.revision + 1);
    let first_receipt_id = format!("userapp-agent:{first_request}");
    let second_receipt_id = format!("userapp-agent:{second_request}");
    let (a, b) = (first_receipt_id.clone(), second_receipt_id.clone());
    let receipts = db
        .execute(move |mut db| async move {
            let a = crate::db::models::ProjectWriteReceipt::filter_by_request_id(&a)
                .first()
                .exec(&mut db)
                .await?
                .expect("first receipt");
            let b = crate::db::models::ProjectWriteReceipt::filter_by_request_id(&b)
                .first()
                .exec(&mut db)
                .await?
                .expect("second receipt");
            Ok(vec![
                (a.request_id, a.fingerprint, a.outcome),
                (b.request_id, b.fingerprint, b.outcome),
            ])
        })
        .await
        .unwrap();
    let failed = receipts
        .iter()
        .find(|row| row.2 == "superseded")
        .expect("one CAS must lose the captured revision")
        .clone();
    assert_eq!(
        receipts.iter().filter(|row| row.2 == "committed").count(),
        1
    );
    let failed_request = failed.0.strip_prefix("userapp-agent:").unwrap();
    assert_eq!(
        backend
            .register_userapp_agent_session_for_route(
                &captured,
                &sid,
                failed_request,
                deadline(),
                &app_generation
            )
            .await
            .unwrap(),
        first
    );
    assert!(
        backend
            .register_userapp_agent_session_for_route(
                &captured,
                &format!("different-{app}"),
                failed_request,
                deadline(),
                &app_generation
            )
            .await
            .is_err()
    );
    // A physical route retired while logical membership remains cannot satisfy
    // the stronger registration postcondition used by this confirmation path.
    assert!(store.clear_session_one_durable(&app, &sid).await);
    assert!(
        backend
            .register_userapp_agent_session_for_route(
                &captured,
                &sid,
                failed_request,
                deadline(),
                &app_generation
            )
            .await
            .is_err()
    );
    let newer_sid = format!("newer-{app}");
    let newer = backend
        .register_userapp_agent_session_for_route(
            &first,
            &newer_sid,
            &format!("newer-{app}"),
            deadline(),
            &app_generation,
        )
        .await
        .unwrap();
    assert!(
        backend
            .register_userapp_agent_session_for_route(
                &captured,
                &sid,
                failed_request,
                deadline(),
                &app_generation
            )
            .await
            .is_err()
    );
    assert_eq!(
        backend
            .get_userapp_agent_binding(&app, &life, &project)
            .await
            .unwrap()
            .unwrap(),
        newer
    );
    assert!(
        backend
            .clear_userapp_agent_sessions(&newer, &format!("stop-{app}"), deadline())
            .await
            .unwrap()
    );
    assert!(
        backend
            .register_userapp_agent_session_for_route(
                &captured,
                &sid,
                failed_request,
                deadline(),
                &app_generation
            )
            .await
            .is_err()
    );
    let changed_app = app.clone();
    db.execute(move |mut db| async move {
        toasty::sql::statement("UPDATE projects SET generation=$2 WHERE project_id=$1")
            .bind(changed_app)
            .bind(uuid::Uuid::new_v4().to_string())
            .exec(&mut db)
            .await?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(
        backend
            .register_userapp_agent_session_for_route(
                &captured,
                &sid,
                failed_request,
                deadline(),
                &app_generation
            )
            .await
            .is_err()
    );
    let failed_id = failed.0.clone();
    let after = db
        .execute(move |mut db| async move {
            let row = crate::db::models::ProjectWriteReceipt::filter_by_request_id(&failed_id)
                .first()
                .exec(&mut db)
                .await?
                .expect("superseded receipt retained");
            Ok((row.request_id, row.fingerprint, row.outcome))
        })
        .await
        .unwrap();
    assert_eq!(after, failed);
}

#[tokio::test]
async fn userapp_foreign_sid_and_stale_clear_roll_back_without_touching_sibling_sessions() {
    let Some((_dsn, app, life, store, _owner)) = fixture().await else {
        return;
    };
    let backend = ProjectStoreBackend::Postgres(store);
    let a = backend
        .accept_userapp_agent_binding(&app, &life, &format!("a-{app}"), "test-user")
        .await
        .unwrap();
    let b = backend
        .accept_userapp_agent_binding(&app, &life, &format!("b-{app}"), "test-user")
        .await
        .unwrap();
    let sid_a = format!("sid-a-{app}");
    let sid_b = format!("sid-b-{app}");
    let a1 = backend
        .register_userapp_agent_session(&a, &sid_a, &format!("a1-{app}"), deadline())
        .await
        .unwrap();
    let b1 = backend
        .register_userapp_agent_session(&b, &sid_b, &format!("b1-{app}"), deadline())
        .await
        .unwrap();
    assert!(
        backend
            .register_userapp_agent_session(&b1, &sid_a, &format!("foreign-{app}"), deadline())
            .await
            .is_err()
    );
    assert!(
        !backend
            .clear_userapp_agent_sessions(&a, &format!("stale-{app}"), deadline())
            .await
            .unwrap()
    );
    assert!(
        backend
            .clear_userapp_agent_sessions(&a1, &format!("clear-{app}"), deadline())
            .await
            .unwrap()
    );
    assert!(
        backend
            .get_userapp_agent_session(&sid_a)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        backend
            .get_userapp_agent_session(&sid_b)
            .await
            .unwrap()
            .unwrap()
            .0,
        b1
    );
    assert!(backend.get(&app).unwrap().container_info().is_some());
}

#[tokio::test]
async fn userapp_route_generation_changes_cannot_commit_a_dispatched_old_session() {
    let Some((_dsn, app, life, store, db)) = fixture().await else {
        return;
    };
    let backend = ProjectStoreBackend::Postgres(store.clone());
    let project = format!("project-{app}");
    let binding = backend
        .accept_userapp_agent_binding(&app, &life, &project, "test-user")
        .await
        .unwrap();
    let sid = format!("old-route-sid-{app}");
    assert!(
        backend
            .register_userapp_agent_session_for_route(
                &binding,
                &sid,
                &format!("route-race-{app}"),
                deadline(),
                "another-app-generation"
            )
            .await
            .is_err()
    );
    assert!(
        backend
            .get_userapp_agent_session(&sid)
            .await
            .unwrap()
            .is_none()
    );
    let original_generation = store
        .inner()
        .get(&app)
        .unwrap()
        .persistence_identity()
        .generation
        .clone();
    let changed_app = app.clone();
    let changed_generation = uuid::Uuid::new_v4().to_string();
    db.execute(move |mut db| async move {
        toasty::sql::statement("UPDATE projects SET generation=$2 WHERE project_id=$1")
            .bind(changed_app)
            .bind(changed_generation)
            .exec(&mut db)
            .await?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(
        backend
            .register_userapp_agent_session_for_route(
                &binding,
                &sid,
                &format!("route-replaced-{app}"),
                deadline(),
                &original_generation
            )
            .await
            .is_err()
    );
    assert!(
        backend
            .get_userapp_agent_session(&sid)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn retired_userapp_sid_is_unusable_and_lifecycle_invalidation_is_generation_exact() {
    let Some((_dsn, app, life, store, _db)) = fixture().await else {
        return;
    };
    let backend = ProjectStoreBackend::Postgres(store.clone());
    let project = format!("project-{app}");
    let binding = backend
        .accept_userapp_agent_binding(&app, &life, &project, "test-user")
        .await
        .unwrap();
    let sid = format!("sid-{app}");
    backend
        .register_userapp_agent_session(&binding, &sid, &format!("register-{app}"), deadline())
        .await
        .unwrap();
    assert!(store.clear_session_one_durable(&app, &sid).await);
    assert!(
        backend
            .get_userapp_agent_session(&sid)
            .await
            .unwrap()
            .is_none()
    );
    let count = backend
        .invalidate_userapp_agent_bindings(&app, &life, &format!("invalidate-{app}"), deadline())
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(
        backend
            .get_userapp_agent_binding(&app, &life, &project)
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.inner().get(&app).unwrap().container_info().is_some());
    userapp_agent::validate_snapshot(&store.database)
        .await
        .unwrap();
    // Check primary state, not only a local cache marker.
    let app_clone = app.clone();
    assert!(
        database::read(&store.database, move |tx| Box::pin(async move {
            Ok(crate::db::models::Project::filter_by_project_id(&app_clone)
                .first()
                .exec(tx)
                .await?
                .is_some())
        }))
        .await
        .unwrap()
    );
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn postgres_binding_routes_work_with_authoritative_turso_lifecycle() {
    use shared_types::UserAppLifecycleStore;
    let Some((_dsn, app, _life, store, pg)) = fixture().await else {
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let path = std::path::absolute(directory.path().join("lifecycle.db")).unwrap();
    let lifecycle = crate::userapp_lifecycle::TursoUserAppStore::open_exclusive(&path)
        .await
        .unwrap();
    let application = lifecycle.ensure_identity(&app).await.unwrap();
    let app_to_remove = app.clone();
    pg.execute(move |mut db| async move {
        toasty::sql::statement("DELETE FROM userapps WHERE app_id=$1")
            .bind(app_to_remove)
            .exec(&mut db)
            .await?;
        Ok(())
    })
    .await
    .unwrap();
    let backend = ProjectStoreBackend::Postgres(store);
    let binding = backend
        .accept_userapp_agent_binding(
            &app,
            &application.lifecycle_id,
            &format!("project-{app}"),
            "test-user",
        )
        .await
        .unwrap();
    let sid = format!("turso-sid-{app}");
    let current = backend
        .register_userapp_agent_session(&binding, &sid, &format!("turso-request-{app}"), deadline())
        .await
        .unwrap();
    assert_eq!(current.lifecycle_id, application.lifecycle_id);
    assert_eq!(
        backend
            .get_userapp_agent_session(&sid)
            .await
            .unwrap()
            .unwrap()
            .0,
        current
    );
    assert_eq!(
        lifecycle
            .get_application(&app)
            .await
            .unwrap()
            .unwrap()
            .lifecycle_id,
        current.lifecycle_id
    );
}

#[tokio::test]
async fn new_lifecycle_admission_expires_old_logic_and_late_cleanup_preserves_new_owner() {
    let Some((_dsn, app, life, store, _db)) = fixture().await else {
        return;
    };
    let backend = ProjectStoreBackend::Postgres(store);
    let project = format!("project-{app}");
    let old = backend
        .accept_userapp_agent_binding(&app, &life, &project, "test-user")
        .await
        .unwrap();
    let old_sid = format!("old-{app}");
    backend
        .register_userapp_agent_session(&old, &old_sid, &format!("old-reg-{app}"), deadline())
        .await
        .unwrap();
    let new_life = format!("new-{life}");
    let new = backend
        .accept_userapp_agent_binding(&app, &new_life, &project, "test-user")
        .await
        .unwrap();
    assert_ne!(new.binding_generation, old.binding_generation);
    assert!(
        backend
            .get_userapp_agent_session(&old_sid)
            .await
            .unwrap()
            .is_none()
    );
    let new_sid = format!("new-{app}");
    let current = backend
        .register_userapp_agent_session(&new, &new_sid, &format!("new-reg-{app}"), deadline())
        .await
        .unwrap();
    assert_eq!(
        backend
            .invalidate_userapp_agent_bindings(
                &app,
                &life,
                &format!("late-cleanup-{app}"),
                deadline()
            )
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        backend
            .get_userapp_agent_session(&new_sid)
            .await
            .unwrap()
            .unwrap()
            .0,
        current
    );
}

#[tokio::test]
async fn project_v1_upgrade_preserves_old_physical_routes_without_importing_legacy_sids() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let (client, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    let task = tokio::spawn(connection);
    let schema = format!("agentupgrade{}", uuid::Uuid::new_v4().simple());
    client
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await
        .unwrap();
    let separator = if dsn.contains('?') { '&' } else { '?' };
    let config = PostgresConfig {
        url: Some(format!("{dsn}{separator}options=-csearch_path%3D{schema}")),
        ..Default::default()
    };
    let owner = crate::db::postgres::open(&config, vec![Component::Project])
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
    // Reconstruct the actual released v1 catalog and keep its ledger unchanged.
    client.batch_execute(&format!("SET search_path TO {schema}; DROP TABLE userapp_agent_bindings,userapp_agent_sessions; DELETE FROM rcoder_schema_migrations WHERE component='project' AND version=2; INSERT INTO containers(container_name,container_generation,container_id,logical_id,service_type,container_ip,internal_port,external_port,status,service_url,last_activity_at_us,created_at_us,row_revision) VALUES('legacy-builder','old-container-generation','old-pod','kept-app','user-app-builder','10.42.0.3',50051,0,'Running','http://legacy-builder:50051',1,1,1); INSERT INTO projects(project_id,generation,container_name,container_generation,service_type,payload_version,last_activity_at_us,created_at_us,row_revision) VALUES('kept-app','old-project-generation','legacy-builder','old-container-generation','user-app-builder',1,1,1,1); INSERT INTO sessions(session_id,generation,project_id,project_generation,created_at_us,last_seen_at_us) VALUES('old-legacy-sid','old-session-generation','kept-app','old-project-generation',1,1); UPDATE projects SET latest_session='old-legacy-sid' WHERE project_id='kept-app';")).await.unwrap();
    let query = "SELECT checksum,schema_fingerprint FROM rcoder_schema_migrations WHERE component='project' AND version=1";
    let row = client.query_one(query, &[]).await.unwrap();
    let before: (String, String) = (row.get(0), row.get(1));
    let store = PgStore::connect(&config, "upgrade-test".into(), "cluster.local".into())
        .await
        .unwrap();
    let backend = ProjectStoreBackend::Postgres(Arc::new(store));
    let route = backend
        .get_by_session_with_fetch("old-legacy-sid")
        .await
        .unwrap();
    assert_eq!(route.project_id(), "kept-app");
    assert_eq!(
        route.persistence_identity().generation,
        "old-project-generation"
    );
    assert_eq!(route.container_info().unwrap().container_id, "old-pod");
    assert!(
        backend
            .get_userapp_agent_session("old-legacy-sid")
            .await
            .unwrap()
            .is_none()
    );
    let row = client.query_one(query, &[]).await.unwrap();
    assert_eq!(before, (row.get(0), row.get(1)));
    let versions: Vec<i64> = client.query("SELECT version FROM rcoder_schema_migrations WHERE component='project' ORDER BY version", &[]).await.unwrap().iter().map(|row| row.get(0)).collect();
    assert_eq!(versions, vec![1, 2]);
    let count: i64 = client
        .query_one("SELECT count(*) FROM userapp_agent_sessions", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "pre-upgrade SID ownership must not be invented");
    assert!(
        backend
            .postgres()
            .unwrap()
            .shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
    drop(backend);
    client
        .batch_execute(&format!(
            "SET search_path TO public; DROP SCHEMA {schema} CASCADE"
        ))
        .await
        .unwrap();
    drop(client);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn lifecycle_invalidation_cleans_a_late_member_only_in_the_captured_binding_generation() {
    let Some((_dsn, app, life, store, _db)) = fixture().await else {
        return;
    };
    let backend = ProjectStoreBackend::Postgres(store.clone());
    let binding = backend
        .accept_userapp_agent_binding(&app, &life, &format!("project-{app}"), "test-user")
        .await
        .unwrap();
    // Capture the lifecycle target before an already-dispatched chat reply arrives.
    let command = crate::userapp_agent::MappingCommand::Invalidate {
        app_id: app.clone(),
        lifecycle_id: life,
        bindings: vec![binding.clone()],
        sessions: Vec::new(),
    };
    let sid = format!("late-sid-{app}");
    backend
        .register_userapp_agent_session(&binding, &sid, &format!("late-reg-{app}"), deadline())
        .await
        .unwrap();
    let op = PersistOp::UserAppAgentMapping {
        request_id: format!("captured-cleanup-{app}"),
        command: Box::new(command),
    };
    assert_eq!(
        writer::execute_registered(&store.database, vec![op])
            .await
            .unwrap(),
        0
    );
    assert!(
        backend
            .get_userapp_agent_session(&sid)
            .await
            .unwrap()
            .is_none()
    );
    assert!(backend.get(&app).unwrap().container_info().is_some());
}
