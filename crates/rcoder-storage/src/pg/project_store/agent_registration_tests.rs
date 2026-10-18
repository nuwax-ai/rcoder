//! Real PostgreSQL tests for concurrent chat completion and owned replacement.
use crate::{
    config::PostgresConfig,
    pg::{PgStore, test_support},
};
use shared_types::{
    AgentWorkspaceIdentity, ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore, ServiceType,
    VerifiedAgentReplacement, persistence::PersistenceWriteOutcome,
};
use std::{sync::Arc, time::Duration};

#[path = "agent_registration_tests/same_workload_pod.rs"]
mod same_workload_pod;

#[path = "agent_registration_tests/registration_refresh.rs"]
mod registration_refresh;

#[path = "agent_registration_tests/cross_replica_existing_session.rs"]
mod cross_replica_existing_session;

#[path = "agent_registration_tests/cross_replica_readback.rs"]
mod cross_replica_readback;

#[path = "agent_registration_tests/stop_generation.rs"]
mod stop_generation;

#[path = "agent_registration_tests/container_intent.rs"]
mod container_intent;

#[path = "agent_registration_tests/legacy_pending.rs"]
mod legacy_pending;

fn basic(owner: &str) -> ContainerBasicInfo {
    ContainerBasicInfo {
        container_id: format!("old-pod-{owner}"),
        container_name: format!("computer-{owner}"),
        container_ip: "10.0.0.1".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: owner.to_owned(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: format!("http://computer-{owner}"),
        workload_uid: Some(format!("old-sts-{owner}")),
    }
}
fn project(id: &str, owner: &str, basic: &ContainerBasicInfo) -> Arc<ProjectAndContainerInfo> {
    let mut project = ProjectAndContainerInfo::new(id.to_owned());
    project.set_service_type(Some(ServiceType::ComputerAgentRunner));
    project.set_user_id(Some(owner.to_owned()));
    project.set_container(Some(basic.clone()));
    Arc::new(project)
}
fn frozen(project: &ProjectAndContainerInfo) -> ProjectAndContainerInfo {
    let mut frozen = project.clone();
    frozen.set_container(project.container_info());
    frozen
}
fn evidence(previous: &ContainerBasicInfo) -> VerifiedAgentReplacement {
    let mut replacement = previous.clone();
    replacement.container_id = format!("new-pod-{}", previous.project_id);
    replacement.workload_uid = Some(format!("new-sts-{}", previous.project_id));
    replacement.container_ip = "10.0.0.2".into();
    VerifiedAgentReplacement {
        identifier: previous.project_id.clone(),
        service_type: ServiceType::ComputerAgentRunner,
        previous: Some(previous.clone()),
        replacement,
        workspace: AgentWorkspaceIdentity {
            namespace: "isolated-test".into(),
            claim_name: "workspace".into(),
            claim_uid: "owned-pvc-uid".into(),
            mount_path: "/home/user".into(),
            sub_path: None,
        },
    }
}
async fn store(dsn: &str) -> PgStore {
    PgStore::connect(
        &PostgresConfig {
            url: Some(dsn.to_owned()),
            ..Default::default()
        },
        "isolated-test".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap()
    .0
}

#[tokio::test]
async fn completion_appends_without_overwriting_concurrent_metadata_and_sessions() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("completion-{}", test_support::uuid_suffix());
    let basic = basic(&id);
    let writer = store(&dsn).await;
    let first_sid = format!("first-{id}");
    writer
        .insert_with_session_durable(id.clone(), project(&id, &id, &basic), &first_sid)
        .await
        .unwrap();
    let expected = frozen(&writer.get(&id).unwrap());
    let reader = store(&dsn).await; // Already connected: immediate SSE needs SQL backfill.
    let mut newer = (*writer.get(&id).unwrap()).clone();
    newer.set_request_id(Some("newer-request".into()));
    newer.set_model_provider(Some(shared_types::ModelProviderConfig {
        id: "newer-model".into(),
        name: "protocol-fixture".into(),
        base_url: "https://model.invalid".into(),
        api_key: "test-only-not-a-secret".into(),
        requires_openai_auth: false,
        default_model: "fixture".into(),
        api_protocol: None,
        wire_api: None,
    }));
    let concurrent_sid = format!("concurrent-{id}");
    writer
        .insert_with_session_durable(id.clone(), Arc::new(newer), &concurrent_sid)
        .await
        .unwrap();
    writer.update_agent_status(&id, 1, "current status");
    let completion_sid = format!("completion-{id}");
    assert_eq!(
        writer
            .append_session_for_container_durable(&expected, &completion_sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let current = writer.get(&id).unwrap();
    assert_eq!(current.request_id(), Some("newer-request"));
    assert_eq!(current.model_provider().unwrap().id, "newer-model");
    assert!(current.sessions().contains(&first_sid));
    assert!(current.sessions().contains(&concurrent_sid));
    assert!(current.sessions().contains(&completion_sid));
    let resolved = reader
        .get_by_session_id_with_fetch_result(&completion_sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.request_id(), Some("newer-request"));
    assert_eq!(resolved.model_provider().unwrap().id, "newer-model");
    assert_eq!(
        resolved.container_info().unwrap().container_id,
        basic.container_id
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn sql_session_lookup_is_not_blocked_by_an_unrelated_pending_write() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("fetch-pending-{}", test_support::uuid_suffix());
    let writer = store(&dsn).await;
    let reader = store(&dsn).await;
    let sid = format!("session-{id}");
    writer
        .insert_with_session_durable(id.clone(), project(&id, &id, &basic(&id)), &sid)
        .await
        .unwrap();
    let unrelated = reader.register_durable(vec![super::persist_ops::PersistOp::TouchProject {
        project_id: format!("unrelated-{id}"),
        generation: "another-generation".into(),
        last_activity: chrono::Utc::now(),
    }]);
    assert!(
        reader
            .pending_ops
            .load(std::sync::atomic::Ordering::Acquire)
            > 0
    );
    let resolved = reader
        .get_by_session_id_with_fetch_result(&sid)
        .await
        .unwrap()
        .expect("persisted routing is valid even if cache publication is deferred");
    assert_eq!(resolved.project_id(), id);
    assert!(
        reader.get_by_session_id(&sid).is_none(),
        "do not overwrite the mirror while it has pending intents"
    );
    drop(unrelated);
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn owned_shared_replacement_preserves_sessions_and_fences_old_writes() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("replacement-owner-{suffix}");
    let a = format!("replacement-a-{suffix}");
    let b = format!("replacement-b-{suffix}");
    let sid_a = format!("session-a-{suffix}");
    let sid_b = format!("session-b-{suffix}");
    let original_basic = basic(&owner);
    let store = store(&dsn).await;
    store
        .insert_with_session_durable(a.clone(), project(&a, &owner, &original_basic), &sid_a)
        .await
        .unwrap();
    store
        .insert_with_session_durable(b.clone(), project(&b, &owner, &original_basic), &sid_b)
        .await
        .unwrap();
    let expected_a = frozen(&store.get(&a).unwrap());
    let expected_b = frozen(&store.get(&b).unwrap());
    let proof = evidence(&original_basic);
    assert_eq!(
        store
            .replace_agent_container_registration_durable(&expected_a, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let migrated_a = store.get(&a).unwrap();
    assert_eq!(
        store
            .get(&b)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        original_basic.container_id,
        "B retains a real predecessor snapshot until its own registration migrates"
    );
    assert_ne!(
        migrated_a.persistence_identity().generation,
        expected_a.persistence_identity().generation
    );
    assert_ne!(
        migrated_a.persistence_identity().sessions[&sid_a],
        expected_a.persistence_identity().sessions[&sid_a]
    );
    assert_eq!(
        store
            .replace_agent_container_registration_durable(&expected_b, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let migrated_b = store.get(&b).unwrap();
    assert_eq!(
        migrated_a
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation,
        migrated_b
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation
    );
    assert_eq!(
        store
            .replace_agent_container_registration_durable(&expected_a, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed,
        "same physical result is idempotent"
    );
    assert_eq!(
        store
            .append_session_for_container_durable(&expected_a, "late-old-runner")
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    let outcomes = super::writer::execute_registered(
        &store.database,
        vec![
            super::persist_ops::PersistOp::RemoveSession {
                session_id: sid_a.clone(),
                generation: expected_a.persistence_identity().sessions[&sid_a].clone(),
            },
            super::persist_ops::PersistOp::AddSession {
                project_id: a.clone(),
                session_id: format!("late-old-{suffix}"),
                project_generation: expected_a.persistence_identity().generation.clone(),
                generation: "old-late-session".into(),
                predecessor: None,
            },
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        outcomes, 2,
        "old deletion and old append target retired identities"
    );
    let reader = self::store(&dsn).await;
    for (project, sid) in [(&a, &sid_a), (&b, &sid_b)] {
        let resolved = reader
            .get_by_session_id_with_fetch_result(sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.project_id(), project);
        assert_eq!(
            resolved.container_info().unwrap().container_id,
            proof.replacement.container_id
        );
    }
    assert!(reader.get_by_session_id("late-old-runner").is_none());
    assert!(
        reader
            .get_by_session_id(&format!("late-old-{suffix}"))
            .is_none()
    );
    assert!(store.remove(&b).is_some());
    assert_eq!(
        store
            .inner()
            .get_all_container_records()
            .iter()
            .find(|c| c.container_name == proof.replacement.container_name)
            .unwrap()
            .container_id,
        proof.replacement.container_id
    );
    assert_eq!(
        store
            .get_by_session_id(&sid_a)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        proof.replacement.container_id
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn retired_project_and_foreign_evidence_cannot_be_rebound() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("replacement-retired-{}", test_support::uuid_suffix());
    let original = basic(&id);
    let store = store(&dsn).await;
    store
        .insert(id.clone(), project(&id, &id, &original))
        .unwrap();
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let expected = frozen(&store.get(&id).unwrap());
    let mut foreign = evidence(&original);
    foreign.replacement.project_id = "another-owner".into();
    assert!(
        store
            .replace_agent_container_registration_durable(&expected, &foreign)
            .await
            .is_err()
    );
    assert_eq!(store.get(&id).unwrap().container_info().unwrap(), original);
    store.remove_durable(&id).await.unwrap();
    assert_eq!(
        store
            .replace_agent_container_registration_durable(&expected, &evidence(&original))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    assert_eq!(
        store
            .append_session_for_container_durable(&expected, "late-after-stop")
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    assert!(store.get(&id).is_none());
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn detached_shared_project_recovers_without_inventing_a_predecessor() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("detached-owner-{suffix}");
    let a = format!("detached-a-{suffix}");
    let b = format!("detached-b-{suffix}");
    let sid_a = format!("detached-session-a-{suffix}");
    let sid_b = format!("detached-session-b-{suffix}");
    let original = basic(&owner);
    let first = store(&dsn).await;
    first
        .insert_with_session_durable(a.clone(), project(&a, &owner, &original), &sid_a)
        .await
        .unwrap();
    first
        .insert_with_session_durable(b.clone(), project(&b, &owner, &original), &sid_b)
        .await
        .unwrap();
    let old_b = frozen(&first.get(&b).unwrap());
    let expected_a = frozen(&first.get(&a).unwrap());
    let proof = evidence(&original);
    assert_eq!(
        first
            .replace_agent_container_registration_durable(&expected_a, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    // A newly started replica sees B's real detached SQL row, with no old UID
    // remaining. Its new request carries fresh ownership, not guessed history.
    let restarted = store(&dsn).await;
    let detached = frozen(&restarted.get(&b).unwrap());
    assert!(detached.container_info().is_none());
    let mut fresh = proof.clone();
    fresh.previous = None;
    assert_eq!(
        restarted
            .replace_agent_container_registration_durable(&detached, &fresh)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let recovered = restarted.get(&b).unwrap();
    assert_ne!(
        recovered.persistence_identity().generation,
        old_b.persistence_identity().generation
    );
    assert_eq!(
        recovered.container_info().unwrap().container_id,
        proof.replacement.container_id
    );
    assert!(recovered.sessions().contains(&sid_b));
    let retired_ops = vec![
        super::persist_ops::PersistOp::RemoveSession {
            session_id: sid_b.clone(),
            generation: old_b.persistence_identity().sessions[&sid_b].clone(),
        },
        super::persist_ops::PersistOp::AddSession {
            project_id: b.clone(),
            session_id: format!("old-detached-late-{suffix}"),
            project_generation: old_b.persistence_identity().generation.clone(),
            generation: "retired-sid".into(),
            predecessor: None,
        },
        super::persist_ops::PersistOp::TouchProject {
            project_id: b.clone(),
            generation: old_b.persistence_identity().generation.clone(),
            last_activity: chrono::Utc::now() + chrono::Duration::days(1),
        },
        super::persist_ops::PersistOp::RemoveProject {
            project_id: b.clone(),
            generation: old_b.persistence_identity().generation.clone(),
        },
    ];
    assert_eq!(
        super::writer::execute_registered(&restarted.database, retired_ops)
            .await
            .unwrap(),
        4
    );
    let reader = store(&dsn).await;
    assert_eq!(
        reader
            .get_by_session_id_with_fetch_result(&sid_b)
            .await
            .unwrap()
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        proof.replacement.container_id
    );
    assert!(
        reader
            .get_by_session_id(&format!("old-detached-late-{suffix}"))
            .is_none()
    );
    assert!(
        reader.get(&b).unwrap().last_activity() < chrono::Utc::now() + chrono::Duration::hours(1)
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        restarted
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn unavailable_database_is_an_error_instead_of_a_session_miss() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let store = store(&dsn).await;
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
    store.database.shutdown().await.unwrap();
    assert!(
        store
            .get_by_session_id_with_fetch_result("unknown-after-db-shutdown")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn deferred_append_requires_authoritative_database_confirmation() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("deferred-append-{}", test_support::uuid_suffix());
    let original = basic(&id);
    let sid = format!("deferred-session-{id}");
    let store = store(&dsn).await;
    store
        .insert(id.clone(), project(&id, &id, &original))
        .unwrap();
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let expected = frozen(&store.get(&id).unwrap());
    let (release, barrier) = hold_advisory_lock(&dsn, format!("project:{id}")).await;
    assert!(matches!(
        store
            .append_session_for_container_durable(&expected, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Deferred { .. }
    ));
    let local_visible_before_commit = store.get_by_session_id(&sid).is_some();
    assert!(
        !store
            .is_session_registration_committed(&expected, &sid)
            .await
            .unwrap()
    );
    let fetched_sid = sid.clone();
    let persisted_before_commit = super::database::read(&store.database, move |tx| {
        Box::pin(async move { super::repo::fetch_project_by_session(tx, &fetched_sid).await })
    })
    .await
    .unwrap()
    .is_some();
    release.send(()).unwrap();
    barrier.await.unwrap();
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let reader = self::store(&dsn).await;
    assert!(
        store
            .is_session_registration_committed(&expected, &sid)
            .await
            .unwrap()
    );
    assert!(
        reader
            .get_by_session_id_with_fetch_result(&sid)
            .await
            .unwrap()
            .is_some(),
        "original immutable fallback must still commit without resending chat"
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        !persisted_before_commit,
        "real SQL lock proves the append was not committed"
    );
    assert!(
        local_visible_before_commit,
        "the local mirror records acceptance while SQL confirmation remains separate"
    );
}

#[tokio::test]
async fn replacement_preserves_an_accepted_registration_before_database_dispatch() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("pending-before-replace-{}", test_support::uuid_suffix());
    let original = basic(&id);
    let store = store(&dsn).await;
    let original_sid = format!("original-{id}");
    store
        .insert_with_session_durable(id.clone(), project(&id, &id, &original), &original_sid)
        .await
        .unwrap();
    let pending_sid = format!("accepted-not-dispatched-{id}");
    // Pause the real immutable registered operation at the boundary between
    // local acceptance and database dispatch. Dropping it uses the production
    // cancellation fallback, so its old generation will execute after replace.
    let pending = {
        let registration = store.registration.lock().unwrap();
        let mut updated = (*store.get(&id).unwrap()).clone();
        updated.set_request_id(Some("accepted-newer-request".into()));
        let mut updated = store
            .prepare_info(Arc::new(updated), &registration)
            .unwrap();
        // Match insert_with_session_durable's admission order: prepare the
        // project identity first, then allocate the accepted session identity.
        Arc::make_mut(&mut updated).add_session(&pending_sid);
        store.inner.insert(id.clone(), updated.clone()).unwrap();
        let op = super::persist_ops::registration_for_info(&updated).unwrap();
        store.register_durable(vec![op])
    };
    let expected = frozen(&store.get(&id).unwrap());
    assert_eq!(
        store
            .replace_agent_container_registration_durable(&expected, &evidence(&original))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    drop(pending);
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let reader = self::store(&dsn).await;
    let resolved = reader
        .get_by_session_id_with_fetch_result(&pending_sid)
        .await
        .unwrap()
        .expect(
            "accepted current metadata and session must transfer to the replacement generation",
        );
    assert_eq!(resolved.request_id(), Some("accepted-newer-request"));
    assert!(resolved.sessions().contains(&original_sid));
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn accepted_stop_retires_a_pending_append_without_resurrection() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("pending-append-stop-{}", test_support::uuid_suffix());
    let original = basic(&id);
    let sid = format!("stopped-session-{id}");
    let store = store(&dsn).await;
    store
        .insert(id.clone(), project(&id, &id, &original))
        .unwrap();
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let expected = frozen(&store.get(&id).unwrap());
    let (release, barrier) = hold_advisory_lock(&dsn, format!("project:{id}")).await;
    assert!(matches!(
        store
            .append_session_for_container_durable(&expected, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Deferred { .. }
    ));
    assert!(
        store.get_by_session_id(&sid).is_some(),
        "append was admitted before stop"
    );
    store.clear_session_durable(&id).await;
    assert!(
        store.get_by_session_id(&sid).is_none(),
        "accepted stop retires the pending identity immediately"
    );
    release.send(()).unwrap();
    barrier.await.unwrap();
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    assert!(
        !store
            .is_session_registration_committed(&expected, &sid)
            .await
            .unwrap()
    );
    let reader = self::store(&dsn).await;
    assert!(
        reader
            .get_by_session_id_with_fetch_result(&sid)
            .await
            .unwrap()
            .is_none(),
        "late original write and fallback cannot undo accepted stop"
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
}

async fn hold_advisory_lock(
    dsn: &str,
    key: String,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let owner = test_support::database(dsn).await;
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
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
                locked_tx.send(()).unwrap();
                let _ = release_rx.await;
                tx.rollback().await?;
                Ok(())
            })
            .await
            .unwrap();
        owner.shutdown().await.unwrap();
    });
    locked_rx.await.unwrap();
    (release_tx, task)
}

#[tokio::test]
async fn accepted_session_clear_during_replacement_removes_the_migrated_generation() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("clear-during-replace-{}", test_support::uuid_suffix());
    let original = basic(&id);
    let sid = format!("cleared-{id}");
    let store = Arc::new(store(&dsn).await);
    store
        .insert_with_session_durable(id.clone(), project(&id, &id, &original), &sid)
        .await
        .unwrap();
    let expected = frozen(&store.get(&id).unwrap());
    let key = format!("session:{sid}");
    let (release, barrier) = hold_advisory_lock(&dsn, key.clone()).await;
    let replacing = {
        let store = store.clone();
        let evidence = evidence(&original);
        tokio::spawn(async move {
            store
                .replace_agent_container_registration_durable(&expected, &evidence)
                .await
        })
    };
    let observer = test_support::database(&dsn).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiters = test_support::optional_text(&observer,
                "SELECT count(*)::text FROM pg_locks WHERE locktype='advisory' AND NOT granted AND classid::bigint=((hashtextextended($1,719324)>>32)&4294967295) AND objid::bigint=(hashtextextended($1,719324)&4294967295) AND objsubid=1", &key).await.unwrap();
            if waiters != "0" { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("replacement reached the real session advisory lock before clear is accepted");
    assert!(store.clear_session_one_durable(&id, &sid).await);
    assert!(!store.get(&id).unwrap().sessions().contains(&sid));
    release.send(()).unwrap();
    barrier.await.unwrap();
    assert_eq!(
        replacing.await.unwrap().unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let reader = self::store(&dsn).await;
    assert!(
        reader
            .get_by_session_id_with_fetch_result(&sid)
            .await
            .unwrap()
            .is_none(),
        "current user's accepted clear must apply to the migrated generation, not be lost as an old clear"
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
    observer.shutdown().await.unwrap();
}

#[tokio::test]
async fn accepted_project_retirement_during_replacement_does_not_reappear() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("retire-during-replace-{}", test_support::uuid_suffix());
    let original = basic(&id);
    let sid = format!("retired-during-replace-session-{id}");
    let store = Arc::new(store(&dsn).await);
    store
        .insert_with_session_durable(id.clone(), project(&id, &id, &original), &sid)
        .await
        .unwrap();
    let expected = frozen(&store.get(&id).unwrap());
    let key = format!("session:{sid}");
    let (release, barrier) = hold_advisory_lock(&dsn, key.clone()).await;
    let replacing = {
        let store = store.clone();
        let evidence = evidence(&original);
        tokio::spawn(async move {
            store
                .replace_agent_container_registration_durable(&expected, &evidence)
                .await
        })
    };
    let observer = test_support::database(&dsn).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiters = test_support::optional_text(&observer,
                "SELECT count(*)::text FROM pg_locks WHERE locktype='advisory' AND NOT granted AND classid::bigint=((hashtextextended($1,719324)>>32)&4294967295) AND objid::bigint=(hashtextextended($1,719324)&4294967295) AND objsubid=1", &key).await.unwrap();
            if waiters != "0" { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("replacement reached session lock before current project retirement is accepted");
    assert!(store.remove_durable(&id).await.is_some());
    assert!(store.get(&id).is_none());
    release.send(()).unwrap();
    barrier.await.unwrap();
    assert_eq!(
        replacing.await.unwrap().unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let reader = self::store(&dsn).await;
    assert!(
        reader.get(&id).is_none(),
        "accepted current project retirement must survive the registration generation replacement"
    );
    assert!(
        reader
            .get_by_session_id_with_fetch_result(&sid)
            .await
            .unwrap()
            .is_none()
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
    observer.shutdown().await.unwrap();
}

#[tokio::test]
async fn delayed_session_fetch_cannot_roll_back_an_already_registered_container_generation() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("fetch-race-owner-{suffix}");
    let a = format!("fetch-race-a-{suffix}");
    let b = format!("fetch-race-b-{suffix}");
    let sid_a = format!("fetch-race-sid-a-{suffix}");
    let sid_b = format!("fetch-race-sid-b-{suffix}");
    let original = basic(&owner);
    let store = store(&dsn).await;
    store
        .insert_with_session_durable(a.clone(), project(&a, &owner, &original), &sid_a)
        .await
        .unwrap();
    store
        .insert_with_session_durable(b.clone(), project(&b, &owner, &original), &sid_b)
        .await
        .unwrap();
    super::sync::sync_once(&store, store.inner(), &store.database)
        .await
        .unwrap();
    let (release, barrier) =
        hold_advisory_lock(&dsn, format!("container-name:{}", original.container_name)).await;
    let mut newer = (*store.get(&a).unwrap()).clone();
    let mut replacement = original.clone();
    replacement.container_id = format!("new-same-workload-pod-{suffix}");
    replacement.container_ip = "10.0.0.2".into();
    newer.set_container(Some(replacement.clone()));
    store.insert(a.clone(), Arc::new(newer)).unwrap();
    let baseline: std::collections::HashMap<_, _> = store.inner.iter().into_iter().collect();
    let baseline_containers = store.container_registration.lock().unwrap().clone();
    let sid = sid_b.clone();
    let fetched = super::database::read(&store.database, move |tx| {
        Box::pin(async move { super::repo::fetch_project_by_session(tx, &sid).await })
    })
    .await
    .unwrap()
    .unwrap();
    // This is the exact production fetch/hydrate boundary: the query saw the
    // predecessor, then its return was delayed while the pending write committed.
    release.send(()).unwrap();
    barrier.await.unwrap();
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let before = store.container_registration.lock().unwrap().clone();
    let resolved = store
        .hydrate_fetched(&sid_b, fetched, &baseline, &baseline_containers)
        .unwrap();
    assert!(
        resolved.as_ref().is_none_or(|info| info
            .container_info()
            .is_some_and(|c| c.container_id == replacement.container_id)),
        "old SQL responses cannot route the retired predecessor"
    );
    assert_eq!(
        *store.container_registration.lock().unwrap(),
        before,
        "old SQL responses cannot roll back a newer admitted container cursor"
    );
    assert_eq!(
        store
            .get(&a)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        replacement.container_id
    );
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn historical_normal_project_and_computer_project_share_replacement_without_semantic_rewrite()
{
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    for container_type in [
        ServiceType::ComputerAgentRunner,
        ServiceType::ComputerNormalProject,
    ] {
        let suffix = test_support::uuid_suffix();
        let owner = format!("normal-owner-{suffix}");
        let a = format!("computer-project-{suffix}");
        let b = format!("normal-project-{suffix}");
        let sid_a = format!("computer-session-{suffix}");
        let sid_b = format!("normal-session-{suffix}");
        let original = basic(&owner);
        let initial = store(&dsn).await;
        initial
            .insert_with_session_durable(a.clone(), project(&a, &owner, &original), &sid_a)
            .await
            .unwrap();
        initial
            .insert_with_session_durable(b.clone(), project(&b, &owner, &original), &sid_b)
            .await
            .unwrap();
        let historical_b = b.clone();
        let container_name = original.container_name.clone();
        initial
            .database
            .execute(move |mut db| async move {
                let mut tx = db.transaction().await?;
                // The production setter normalizes registry families. Historical
                // SQL may retain the original enum spelling, so use real stored
                // rows to exercise that compatibility rather than a fake setter.
                toasty::sql::statement("UPDATE projects SET service_type=$1 WHERE project_id=$2")
                    .bind(ServiceType::ComputerNormalProject.to_string())
                    .bind(historical_b)
                    .exec(&mut tx)
                    .await?;
                toasty::sql::statement(
                    "UPDATE containers SET service_type=$1 WHERE container_name=$2",
                )
                .bind(container_type.to_string())
                .bind(container_name)
                .exec(&mut tx)
                .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
            .unwrap();
        let repaired = store(&dsn).await;
        let expected_a = frozen(&repaired.get(&a).unwrap());
        let expected_b = frozen(&repaired.get(&b).unwrap());
        let proof = evidence(&original);
        assert_eq!(
            repaired
                .replace_agent_container_registration_durable(&expected_a, &proof)
                .await
                .unwrap(),
            PersistenceWriteOutcome::Committed
        );
        let mut normal_proof = proof.clone();
        normal_proof.service_type = ServiceType::ComputerNormalProject;
        assert_eq!(
            repaired
                .replace_agent_container_registration_durable(&expected_b, &normal_proof)
                .await
                .unwrap(),
            PersistenceWriteOutcome::Committed
        );
        assert_eq!(
            test_support::optional_text(
                &repaired.database,
                "SELECT service_type FROM projects WHERE project_id=$1",
                &b
            )
            .await
            .as_deref(),
            Some("computer-normal-project")
        );
        assert_eq!(
            test_support::optional_text(
                &repaired.database,
                "SELECT service_type FROM projects WHERE project_id=$1",
                &a
            )
            .await
            .as_deref(),
            Some("computer-agent-runner")
        );
        let later_sid = format!("normal-later-session-{suffix}");
        let current = frozen(&repaired.get(&b).unwrap());
        assert_eq!(
            repaired
                .append_session_for_container_durable(&current, &later_sid)
                .await
                .unwrap(),
            PersistenceWriteOutcome::Committed
        );
        let reader = store(&dsn).await;
        for sid in [&sid_a, &sid_b, &later_sid] {
            assert_eq!(
                reader
                    .get_by_session_id_with_fetch_result(sid)
                    .await
                    .unwrap()
                    .unwrap()
                    .container_info()
                    .unwrap()
                    .container_id,
                proof.replacement.container_id
            );
        }
        assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
        assert!(
            repaired
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
        assert!(
            initial
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
    }
}

async fn computer_mapping_dispatch_race(shared_container: bool) {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("first-dispatch-owner-{suffix}");
    let project_id = format!("first-dispatch-project-{suffix}");
    let session_id = format!("first-dispatch-session-{suffix}");
    let unrelated = format!("unrelated-writer-blocker-{suffix}");
    let store = store(&dsn).await;
    store
        .insert_with_session_durable(
            unrelated.clone(),
            project(&unrelated, &unrelated, &basic(&unrelated)),
            &format!("unrelated-session-{suffix}"),
        )
        .await
        .unwrap();
    let physical = basic(&owner);
    if shared_container {
        let first_project = format!("existing-shared-project-{suffix}");
        store
            .insert_with_session_durable(
                first_project.clone(),
                project(&first_project, &owner, &physical),
                &format!("existing-shared-session-{suffix}"),
            )
            .await
            .unwrap();
    }
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let key = format!("project:{unrelated}");
    let (release, barrier) = hold_advisory_lock(&dsn, key.clone()).await;
    store.update_activity(&unrelated).unwrap();
    let observer = test_support::database(&dsn).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiters = test_support::optional_text(&observer,
                "SELECT count(*)::text FROM pg_locks WHERE locktype='advisory' AND NOT granted AND classid::bigint=((hashtextextended($1,719324)>>32)&4294967295) AND objid::bigint=(hashtextextended($1,719324)&4294967295) AND objsubid=1", &key).await.unwrap();
            if waiters != "0" { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("the real background writer is blocked on another project's batch");
    // The Computer ensure boundary now confirms the registration before a fast
    // runner may receive work. This does not rely on the blocked writer's FIFO.
    let registration_outcome = store
        .insert_project_registration_durable(
            project_id.clone(),
            project(&project_id, &owner, &physical),
        )
        .await
        .unwrap();
    let registered_id = project_id.clone();
    let committed_before_dispatch = super::database::read(&store.database, move |tx| {
        Box::pin(async move { super::repo::project_exists(tx, &registered_id).await })
    })
    .await
    .unwrap();
    let captured = frozen(&store.get(&project_id).unwrap());
    let outcome = store
        .append_session_for_container_durable(&captured, &session_id)
        .await
        .unwrap();
    release.send(()).unwrap();
    barrier.await.unwrap();
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let reader = self::store(&dsn).await;
    let resolved = reader
        .get_by_session_id_with_fetch_result(&session_id)
        .await
        .unwrap();
    assert!(
        reader.get(&project_id).is_some(),
        "the original queued registration eventually committed"
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
    observer.shutdown().await.unwrap();
    assert_eq!(registration_outcome, PersistenceWriteOutcome::Committed);
    assert!(
        committed_before_dispatch,
        "dispatch is gated by an actual primary-database registration"
    );
    assert_eq!(
        outcome,
        PersistenceWriteOutcome::Committed,
        "Computer mapping must commit before task dispatch, independently of the background writer"
    );
    assert_eq!(resolved.unwrap().project_id(), project_id);
}

#[tokio::test]
async fn first_computer_dispatch_cannot_overtake_its_project_registration() {
    computer_mapping_dispatch_race(false).await;
}

#[tokio::test]
async fn shared_second_project_dispatch_cannot_overtake_its_project_registration() {
    computer_mapping_dispatch_race(true).await;
}

#[tokio::test]
async fn new_shared_project_cannot_replace_a_canonical_workload_without_verified_evidence() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("canonical-owner-{suffix}");
    let a = format!("canonical-old-project-{suffix}");
    let b = format!("canonical-new-project-{suffix}");
    let sid_a = format!("canonical-old-session-{suffix}");
    let original = basic(&owner);
    let store = store(&dsn).await;
    store
        .insert_with_session_durable(a.clone(), project(&a, &owner, &original), &sid_a)
        .await
        .unwrap();
    let expected_a = frozen(&store.get(&a).unwrap());
    let different = evidence(&original).replacement;
    let outcome = store.insert(b.clone(), project(&b, &owner, &different));
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let reader = self::store(&dsn).await;
    let persisted_a = reader.get(&a).unwrap();
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        outcome.is_err(),
        "a new project's absence is not permission to replace another project's shared workload"
    );
    assert_eq!(persisted_a.container_info(), expected_a.container_info());
    assert!(
        store.get(&b).is_none(),
        "refused registration cannot leave a mixed new identity / old Basic record"
    );
}

#[tokio::test]
async fn database_rejects_an_unverified_canonical_replacement_from_a_stale_replica() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("stale-canonical-owner-{suffix}");
    let a = format!("stale-canonical-a-{suffix}");
    let b = format!("stale-canonical-b-{suffix}");
    let old = basic(&owner);
    let store = store(&dsn).await;
    store
        .insert_with_session_durable(
            a.clone(),
            project(&a, &owner, &old),
            &format!("old-sid-{suffix}"),
        )
        .await
        .unwrap();
    let original = frozen(&store.get(&a).unwrap());
    let original_identity = original.persistence_identity().container.as_ref().unwrap();
    let different = evidence(&old).replacement;
    // A stale replica may prepare this predecessor CAS without observing the
    // other project's canonical cache. SQL must still require typed recovery.
    let mut incoming = (*project(&b, &owner, &different)).clone();
    let mut identity = incoming.persistence_identity().clone();
    identity.revision = 1;
    identity.container = Some(shared_types::persistence::ContainerPersistenceIdentity {
        generation: uuid::Uuid::new_v4().to_string(),
        revision: 1,
        physical_uid: Some(different.container_id),
        predecessor: Some(original_identity.generation.clone()),
        predecessor_revision: Some(original_identity.revision),
    });
    incoming.set_persistence_identity(identity);
    let unverified = super::persist_ops::registration_for_info(&incoming).unwrap();
    assert_eq!(
        super::writer::execute_registered(&store.database, vec![unverified])
            .await
            .unwrap(),
        1
    );
    let reader = self::store(&dsn).await;
    assert_eq!(
        reader.get(&a).unwrap().container_info(),
        original.container_info()
    );
    assert!(reader.get(&b).is_none());
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn confirmed_registration_is_superseded_by_an_accepted_same_physical_new_project_generation()
{
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("register-retire-generation-{}", test_support::uuid_suffix());
    let physical = basic(&id);
    let store = Arc::new(store(&dsn).await);
    store
        .insert_with_session_durable(
            id.clone(),
            project(&id, &id, &physical),
            &format!("original-{id}"),
        )
        .await
        .unwrap();
    let old_generation = store
        .get(&id)
        .unwrap()
        .persistence_identity()
        .generation
        .clone();
    let key = format!("project:{id}");
    let (release, barrier) = hold_advisory_lock(&dsn, key.clone()).await;
    let registering = {
        let store = store.clone();
        let id = id.clone();
        let mut incoming = (*store.get(&id).unwrap()).clone();
        incoming.set_request_id(Some("request-before-stop".into()));
        tokio::spawn(async move {
            store
                .insert_project_registration_durable(id, Arc::new(incoming))
                .await
        })
    };
    let observer = test_support::database(&dsn).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiters = test_support::optional_text(&observer,
                "SELECT count(*)::text FROM pg_locks WHERE locktype='advisory' AND NOT granted AND classid::bigint=((hashtextextended($1,719324)>>32)&4294967295) AND objid::bigint=(hashtextextended($1,719324)&4294967295) AND objsubid=1", &key).await.unwrap();
            if waiters != "0" { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("registration entered SQL before a same-physical new project is accepted");
    store.remove(&id).unwrap();
    let mut newer = (*project(&id, &id, &physical)).clone();
    newer.set_request_id(Some("new-request-after-stop".into()));
    store.insert(id.clone(), Arc::new(newer)).unwrap();
    let new_generation = store
        .get(&id)
        .unwrap()
        .persistence_identity()
        .generation
        .clone();
    assert_ne!(new_generation, old_generation);
    release.send(()).unwrap();
    barrier.await.unwrap();
    assert_eq!(
        registering.await.unwrap().unwrap(),
        PersistenceWriteOutcome::Superseded,
        "the old request cannot dispatch by observing the newer same-physical project"
    );
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let reader = self::store(&dsn).await;
    let current = reader.get(&id).unwrap();
    assert_eq!(current.persistence_identity().generation, new_generation);
    assert_eq!(current.request_id(), Some("new-request-after-stop"));
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
    observer.shutdown().await.unwrap();
}
