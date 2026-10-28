//! Real PG retirement and admission timing regressions; no background sync runs.
use super::*;
use crate::pg::project_store::authoritative::{AuthoritativeVerificationRequired, GateAuthority};
use shared_types::persistence::PersistenceWriteOutcome;

struct Fixture {
    first: PgStore,
    peer: PgStore,
    id: String,
    old_session: String,
    original: ContainerBasicInfo,
    replacement: ContainerBasicInfo,
}

async fn fixture() -> Option<Fixture> {
    let dsn = test_dsn().await?;
    let id = format!("authority-review-{}", uuid_suffix());
    let old_session = format!("authority-old-session-{id}");
    let mut original = container_for(&id);
    original.workload_uid = Some(format!("old-workload-{id}"));
    let mut replacement = original.clone();
    replacement.container_id = format!("new-physical-{id}");
    replacement.workload_uid = Some(format!("new-workload-{id}"));
    let first = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .unwrap();
    first
        .insert_with_session_durable(
            id.clone(),
            info_for(&id, Some(original.clone())),
            &old_session,
        )
        .await
        .unwrap();
    assert!(first.wait_drained(Duration::from_secs(5)).await);
    let peer = PgStore::connect(
        &PostgresConfig {
            max_connections: Some(1),
            min_connections: Some(1),
            // 钉住 600ms：权威读超时测试断言"exceeded the durable deadline"，
            // 需权威读在 2s 外层内因 durable deadline 超时返回，与运行期默认值解耦。
            durable_commit_timeout_ms: Some(600),
            ..pg_config(&dsn)
        },
        "test-ns".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap();
    Some(Fixture {
        first,
        peer,
        id,
        old_session,
        original,
        replacement,
    })
}

impl Fixture {
    async fn delete_original(&self) {
        assert_eq!(
            self.first
                .delete_container_with_projects_durable(&self.original.container_id)
                .await,
            (true, 1)
        );
        assert!(self.first.wait_drained(Duration::from_secs(5)).await);
    }

    async fn finish(self) {
        assert!(
            self.peer
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
        assert!(
            self.first
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
    }
}

#[tokio::test]
async fn retired_captured_registration_creates_fresh_identity_without_old_sessions() {
    let Some(f) = fixture().await else { return };
    let mut request = (*f.peer.get(&f.id).unwrap()).clone();
    let old_generation = request.persistence_identity().generation.clone();
    assert!(request.sessions().contains(&f.old_session));
    request.set_container(Some(f.replacement.clone()));
    request.set_request_id(Some("fresh-request-after-peer-retirement".into()));
    f.delete_original().await;
    assert_eq!(
        f.peer
            .insert_project_registration_durable(f.id.clone(), Arc::new(request))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let admitted = f.peer.get(&f.id).unwrap();
    assert_ne!(admitted.persistence_identity().generation, old_generation);
    assert_eq!(
        admitted.container_info().unwrap().container_id,
        f.replacement.container_id
    );
    assert_eq!(
        admitted.request_id(),
        Some("fresh-request-after-peer-retirement")
    );
    assert!(admitted.sessions().is_empty());
    assert!(f.peer.get_by_session_id(&f.old_session).is_none());
    let persisted = f
        .peer
        .refresh_project_registration(&f.id)
        .await
        .unwrap()
        .unwrap();
    assert!(persisted.sessions().is_empty());
    assert_eq!(
        persisted.container_info().unwrap().container_id,
        f.replacement.container_id
    );
    f.finish().await;
}

#[tokio::test]
async fn retired_captured_session_registration_keeps_only_the_new_session() {
    let Some(f) = fixture().await else { return };
    let mut request = (*f.peer.get(&f.id).unwrap()).clone();
    request.set_container(Some(f.replacement.clone()));
    f.delete_original().await;
    let new_session = format!("new-session-{}", f.id);
    f.peer
        .insert_with_session_durable(f.id.clone(), Arc::new(request), &new_session)
        .await
        .unwrap();
    assert!(f.peer.wait_drained(Duration::from_secs(5)).await);
    let persisted = f
        .peer
        .refresh_project_registration(&f.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.sessions().len(), 1);
    assert!(persisted.sessions().contains(&new_session));
    assert!(f.peer.get_by_session_id(&f.old_session).is_none());
    f.finish().await;
}

#[tokio::test]
async fn authoritative_snapshot_cannot_retire_a_newer_local_registration() {
    let Some(f) = fixture().await else { return };
    f.delete_original().await;
    let read = f
        .peer
        .read_authoritative_container_binding(
            &f.original.container_name,
            &f.id,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    // A local admission advances the exact cursor after the read. Its own SQL
    // outcome must settle it; the older read cannot clear this new intent.
    f.peer
        .insert(f.id.clone(), info_for(&f.id, Some(f.original.clone())))
        .unwrap();
    let current = f.peer.get(&f.id).unwrap();
    {
        let mut registration = f.peer.registration.lock().unwrap();
        let error = f
            .peer
            .prepare_authoritative_info(
                info_for(&f.id, Some(f.replacement.clone())),
                &mut registration,
                &GateAuthority::Snapshot(Box::new(read)),
            )
            .err()
            .unwrap();
        assert!(
            error
                .downcast_ref::<AuthoritativeVerificationRequired>()
                .is_some()
        );
        assert!(Arc::ptr_eq(&current, &f.peer.get(&f.id).unwrap()));
    }
    f.finish().await;
}

#[tokio::test]
async fn authoritative_lookup_respects_the_registration_deadline() {
    let Some(f) = fixture().await else { return };
    f.delete_original().await;
    let database = f.peer.database.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, waiting) = tokio::sync::oneshot::channel();
    let held = tokio::spawn(async move {
        database
            .execute(move |mut db| async move {
                let mut tx = db.transaction().await?;
                toasty::sql::query("SELECT 1").exec(&mut tx).await?;
                started.send(()).unwrap();
                waiting.await.unwrap();
                tx.rollback().await?;
                Ok(())
            })
            .await
            .unwrap();
    });
    ready.await.unwrap();
    let before = f.peer.get(&f.id).unwrap();
    let attempt = tokio::time::timeout(
        Duration::from_secs(2),
        f.peer.insert_project_registration_durable(
            f.id.clone(),
            info_for(&f.id, Some(f.replacement.clone())),
        ),
    )
    .await;
    // Release before asserting so a failed test also drains the real DB owner.
    release.send(()).unwrap();
    held.await.unwrap();
    let error = attempt
        .expect("lookup must be covered by the durable deadline")
        .unwrap_err();
    assert!(
        error.to_string().contains("exceeded the durable deadline"),
        "{error:#}"
    );
    assert!(Arc::ptr_eq(&before, &f.peer.get(&f.id).unwrap()));
    f.finish().await;
}

#[tokio::test]
async fn peer_rebuilt_successor_is_visible_to_the_deleting_replica_without_sync() {
    // Three original entrances: read, captured same-project registration, and
    // a fresh project sharing the already rebuilt runtime. No sync is started.
    for entrance in 0..3 {
        let Some(f) = fixture().await else { return };
        let mut captured = (*f.first.get(&f.id).unwrap()).clone();
        captured.set_container(Some(f.replacement.clone()));
        f.delete_original().await;
        assert_eq!(
            f.peer
                .insert_project_registration_durable(
                    f.id.clone(),
                    info_for(&f.id, Some(f.replacement.clone()))
                )
                .await
                .unwrap(),
            PersistenceWriteOutcome::Committed
        );
        let peer_generation = f
            .peer
            .get(&f.id)
            .unwrap()
            .persistence_identity()
            .generation
            .clone();
        let requested = match entrance {
            0 => f.id.clone(),
            1 => {
                assert_eq!(
                    f.first
                        .insert_project_registration_durable(f.id.clone(), Arc::new(captured))
                        .await
                        .unwrap(),
                    PersistenceWriteOutcome::Committed
                );
                f.id.clone()
            }
            2 => {
                let id = format!("fresh-shared-{}", f.id);
                assert_eq!(
                    f.first
                        .insert_project_registration_durable(
                            id.clone(),
                            info_for(&id, Some(f.replacement.clone()))
                        )
                        .await
                        .unwrap(),
                    PersistenceWriteOutcome::Committed
                );
                id
            }
            _ => unreachable!(),
        };
        let persisted = f
            .first
            .refresh_project_registration(&requested)
            .await
            .unwrap()
            .expect("the real successor must be readable immediately");
        assert_eq!(
            persisted.container_info().unwrap().container_id,
            f.replacement.container_id
        );
        assert_eq!(
            persisted.container_info().unwrap().workload_uid,
            f.replacement.workload_uid
        );
        assert!(persisted.sessions().is_empty());
        assert!(f.first.get_by_session_id(&f.old_session).is_none());
        if requested == f.id {
            assert_eq!(persisted.persistence_identity().generation, peer_generation);
        }
        f.finish().await;
    }
}
