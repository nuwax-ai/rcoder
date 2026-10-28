//! Clear-all is a captured Stop, not a late clear of whichever request won next.
use super::*;
use crate::pg::project_store::{persist_ops::PersistOp, project_stop::ProjectStop};

async fn fixture(dsn: &str) -> (PgStore, String, String, ContainerBasicInfo) {
    let suffix = test_support::uuid_suffix();
    let id = format!("stop-project-epoch-{suffix}");
    let sid = format!("stop-project-sid-{suffix}");
    let physical = basic(&id);
    let first = store(dsn).await;
    first
        .insert_with_session_durable(id.clone(), project(&id, &id, &physical), &sid)
        .await
        .unwrap();
    (first, id, sid, physical)
}

async fn rechat(store: &PgStore, id: &str, sid: &str, request: &str) -> ProjectAndContainerInfo {
    let mut next = (*store.get(id).unwrap()).clone();
    next.set_request_id(Some(request.to_owned()));
    assert_eq!(
        store
            .insert_project_registration_durable(id.to_owned(), Arc::new(next))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let captured = frozen(&store.get(id).unwrap());
    assert_eq!(
        store
            .append_session_for_container_durable(&captured, sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed,
        "one new request can restore the SDK's original SID without replaying its prompt"
    );
    frozen(&store.get(id).unwrap())
}

#[tokio::test]
async fn stop_then_same_sid_new_chat_preserves_the_actual_shared_runner_and_other_project() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let (first, id, sid, physical) = fixture(&dsn).await;
    let other = format!("other-{id}");
    let other_sid = format!("other-{sid}");
    first
        .insert_with_session_durable(other.clone(), project(&other, &id, &physical), &other_sid)
        .await
        .unwrap();
    let other_before = frozen(&first.get(&other).unwrap());
    let frame = first
        .capture_project_stop_registration(&id)
        .await
        .unwrap()
        .unwrap();
    let old = frame.persistence_identity().generation.clone();
    assert_eq!(
        first
            .clear_sessions_if_current_durable(&frame)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let new = first
        .get(&id)
        .unwrap()
        .persistence_identity()
        .generation
        .clone();
    assert_ne!(new, old);
    let after = rechat(&first, &id, &sid, "new-same-sdk-sid").await;
    assert_eq!(after.persistence_identity().generation, new);
    assert_eq!(
        after.container_info().unwrap().container_id,
        physical.container_id
    );
    assert_eq!(
        after.container_info().unwrap().workload_uid,
        physical.workload_uid
    );
    assert_eq!(
        after
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation,
        frame
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation
    );
    let peer = store(&dsn).await;
    let other_after = peer.get(&other).unwrap();
    assert_eq!(
        other_after.persistence_identity().generation,
        other_before.persistence_identity().generation
    );
    assert_eq!(
        other_after.persistence_identity().sessions,
        other_before.persistence_identity().sessions
    );
    assert_eq!(other_after.latest_session(), Some(other_sid.as_str()));
    assert!(
        !first
            .is_session_registration_committed(&frame, &sid)
            .await
            .unwrap()
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn two_replicas_stopping_the_same_source_converge_to_one_committed_epoch() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let (first, id, sid, _) = fixture(&dsn).await;
    let second = store(&dsn).await;
    let a = first
        .capture_project_stop_registration(&id)
        .await
        .unwrap()
        .unwrap();
    let b = second
        .capture_project_stop_registration(&id)
        .await
        .unwrap()
        .unwrap();
    let (one, two) = tokio::join!(
        first.clear_sessions_if_current_durable(&a),
        second.clear_sessions_if_current_durable(&b)
    );
    let outcomes = [one.unwrap(), two.unwrap()];
    assert!(outcomes.contains(&PersistenceWriteOutcome::Committed));
    assert!(outcomes.iter().all(|outcome| matches!(
        outcome,
        PersistenceWriteOutcome::Committed | PersistenceWriteOutcome::Superseded
    )));
    assert!(first.resolve_project_stop(&id).await.unwrap());
    assert!(second.resolve_project_stop(&id).await.unwrap());
    let generation = first
        .get(&id)
        .unwrap()
        .persistence_identity()
        .generation
        .clone();
    assert_ne!(generation, a.persistence_identity().generation);
    assert_eq!(
        second.get(&id).unwrap().persistence_identity().generation,
        generation
    );
    assert!(first.get(&id).unwrap().sessions().is_empty());
    assert!(second.project_stops.lock().unwrap().is_empty());
    rechat(&second, &id, &sid, "after-two-stops").await;
    assert!(second.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn late_original_stop_is_a_noop_after_fresh_same_sid_chat() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let (first, id, sid, _) = fixture(&dsn).await;
    let frame = first
        .capture_project_stop_registration(&id)
        .await
        .unwrap()
        .unwrap();
    let (release, lock) = hold_advisory_lock(&dsn, format!("project:{id}")).await;
    let mut stop = Box::pin(first.clear_sessions_if_current_durable(&frame));
    assert!(futures::poll!(stop.as_mut()).is_pending());
    let original = first.project_stops.lock().unwrap()[&id].clone();
    release.send(()).unwrap();
    lock.await.unwrap();
    assert_eq!(stop.await.unwrap(), PersistenceWriteOutcome::Committed);
    let fresh = rechat(&first, &id, &sid, "newer-than-stop-replay").await;
    assert_eq!(
        crate::pg::project_store::writer::execute_registered(
            &first.database,
            vec![PersistOp::StopProject(Box::new((*original).clone()))]
        )
        .await
        .unwrap(),
        0
    );
    let peer = store(&dsn).await;
    let persisted = peer.get(&id).unwrap();
    assert_eq!(
        persisted.persistence_identity().generation,
        fresh.persistence_identity().generation
    );
    assert_eq!(
        persisted.persistence_identity().sessions,
        fresh.persistence_identity().sessions
    );
    assert_eq!(persisted.latest_session(), Some(sid.as_str()));
    assert_eq!(persisted.request_id(), Some("newer-than-stop-replay"));
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn newer_remote_metadata_supersedes_a_late_captured_stop_even_without_request_id() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let (first, id, sid, _) = fixture(&dsn).await;
    let frame = first
        .capture_project_stop_registration(&id)
        .await
        .unwrap()
        .unwrap();
    assert!(frame.request_id().is_none());
    let second = store(&dsn).await;
    let mut next = (*second.get(&id).unwrap()).clone();
    next.set_request_id(None);
    next.set_model_provider(Some(shared_types::ModelProviderConfig {
        id: "after-stop-frame".into(),
        name: "fixture".into(),
        base_url: "https://model.invalid".into(),
        api_key: "not-a-secret".into(),
        default_model: "fixture".into(),
        requires_openai_auth: false,
        api_protocol: None,
        wire_api: None,
    }));
    assert_eq!(
        second
            .insert_project_registration_durable(id.clone(), Arc::new(next))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(
        first
            .clear_sessions_if_current_durable(&frame)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    let local = first.get(&id).unwrap();
    assert_eq!(
        local.persistence_identity().generation,
        frame.persistence_identity().generation
    );
    assert!(local.sessions().contains(&sid));
    assert_eq!(local.model_provider().unwrap().id, "after-stop-frame");
    assert!(first.project_stops.lock().unwrap().is_empty());
    assert!(second.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn cancelled_stop_keeps_its_immutable_fallback_and_fresh_request_resolves_the_epoch() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let (first, id, sid, _) = fixture(&dsn).await;
    let frame = first
        .capture_project_stop_registration(&id)
        .await
        .unwrap()
        .unwrap();
    let (release, lock) = hold_advisory_lock(&dsn, format!("project:{id}")).await;
    let mut stop = Box::pin(first.clear_sessions_if_current_durable(&frame));
    assert!(futures::poll!(stop.as_mut()).is_pending());
    drop(stop);
    let mut early = (*first.get(&id).unwrap()).clone();
    early.set_request_id(Some("before-stop-confirmed".into()));
    assert!(
        matches!(
            first
                .insert_project_registration_durable(id.clone(), Arc::new(early))
                .await
                .unwrap(),
            PersistenceWriteOutcome::Deferred { .. }
        ),
        "metadata cannot materialize an unconfirmed epoch before the captured Stop CAS"
    );
    release.send(()).unwrap();
    lock.await.unwrap();
    assert!(first.wait_drained(Duration::from_secs(5)).await);
    let current = first
        .capture_project_stop_registration(&id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        current.persistence_identity().generation,
        frame.persistence_identity().generation
    );
    assert!(current.sessions().is_empty());
    rechat(&first, &id, &sid, "after-cancelled-stop-confirmed").await;
    assert!(first.project_stops.lock().unwrap().is_empty());
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn true_physical_replacement_rejects_an_old_stop_frame() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let (first, id, sid, physical) = fixture(&dsn).await;
    let frame = first
        .capture_project_stop_registration(&id)
        .await
        .unwrap()
        .unwrap();
    let old_stop = ProjectStop::capture(&frame).unwrap();
    assert_eq!(
        first
            .replace_agent_container_registration_durable(&frame, &evidence(&physical))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(
        crate::pg::project_store::writer::execute_registered(
            &first.database,
            vec![PersistOp::StopProject(Box::new(old_stop))]
        )
        .await
        .unwrap(),
        1
    );
    assert!(first.get(&id).unwrap().sessions().contains(&sid));
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn stale_replica_fresh_metadata_refreshes_a_retired_source_before_one_same_sid_dispatch() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let (first, id, sid, _) = fixture(&dsn).await;
    let stale = store(&dsn).await;
    let mut incoming = (*stale.get(&id).unwrap()).clone();
    incoming.set_request_id(Some("fresh-after-remote-stop".into()));
    let frame = first
        .capture_project_stop_registration(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first
            .clear_sessions_if_current_durable(&frame)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(
        stale
            .insert_project_registration_durable(id.clone(), Arc::new(incoming.clone()))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    let registered = stale
        .refresh_project_registration(&id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        registered.persistence_identity().generation,
        frame.persistence_identity().generation
    );
    // This is the fresh engine retry sequence: merge the held incoming fields
    // onto the SQL winner, not onto the dispatched old project's frame.
    let mut rebased = (*registered).clone();
    rebased.set_request_id(incoming.request_id().map(str::to_owned));
    assert_eq!(
        stale
            .insert_project_registration_durable(id.clone(), Arc::new(rebased))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let captured = frozen(&stale.get(&id).unwrap());
    assert_eq!(
        stale
            .append_session_for_container_durable(&captured, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(
        stale.get(&id).unwrap().request_id(),
        Some("fresh-after-remote-stop")
    );
    assert_eq!(
        stale
            .append_session_for_container_durable(&frame, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded,
        "the dispatched old completion cannot use the fresh request's rebase path"
    );
    assert!(stale.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn fresh_stop_on_a_stale_replica_captures_the_current_authoritative_epoch() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let (first, id, sid, _) = fixture(&dsn).await;
    let stale = store(&dsn).await;
    let old = first
        .capture_project_stop_registration(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first.clear_sessions_if_current_durable(&old).await.unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let after = rechat(&first, &id, &sid, "between-two-real-stops").await;
    let fresh_stop = stale
        .capture_project_stop_registration(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fresh_stop.persistence_identity().generation,
        after.persistence_identity().generation
    );
    assert_eq!(fresh_stop.request_id(), Some("between-two-real-stops"));
    assert_eq!(
        stale
            .clear_sessions_if_current_durable(&fresh_stop)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let reader = store(&dsn).await;
    let ended = reader.get(&id).unwrap();
    assert!(ended.sessions().is_empty());
    assert_ne!(
        ended.persistence_identity().generation,
        after.persistence_identity().generation
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(stale.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn stop_cannot_advance_the_same_physical_project_for_a_different_owner() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let (first, id, sid, _) = fixture(&dsn).await;
    let mut invalid = frozen(&first.get(&id).unwrap());
    invalid.set_user_id(Some("different-owner".into()));
    let stop = ProjectStop::capture(&invalid).unwrap();
    assert_eq!(
        crate::pg::project_store::writer::execute_registered(
            &first.database,
            vec![PersistOp::StopProject(Box::new(stop))]
        )
        .await
        .unwrap(),
        1
    );
    let reader = store(&dsn).await;
    assert_eq!(
        reader.get(&id).unwrap().persistence_identity().generation,
        first.get(&id).unwrap().persistence_identity().generation
    );
    assert!(reader.get(&id).unwrap().sessions().contains(&sid));
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}
