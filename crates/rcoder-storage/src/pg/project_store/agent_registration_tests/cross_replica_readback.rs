//! SQL-confirmed routing may replace a cancelled lookup's uncommitted UUID.
use super::cross_replica_existing_session::hold_store_connection;
use super::*;

struct CancelledLookupFixture {
    first: PgStore,
    peer: PgStore,
    id: String,
    sid: String,
    canonical: String,
    initial: ProjectAndContainerInfo,
    captured: ProjectAndContainerInfo,
}

async fn cancelled_lookup_fixture(dsn: &str) -> CancelledLookupFixture {
    let suffix = test_support::uuid_suffix();
    let id = format!("canonical-readback-{suffix}");
    let sid = format!("canonical-readback-sid-{suffix}");
    let physical = basic(&id);
    let first = store(dsn).await;
    first
        .insert_project_registration_durable(id.clone(), project(&id, &id, &physical))
        .await
        .unwrap();
    let peer = PgStore::connect(
        &PostgresConfig {
            url: Some(dsn.to_owned()),
            max_connections: Some(2),
            min_connections: Some(2),
            ..Default::default()
        },
        "isolated-test".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap()
    .0;
    let initial = frozen(&first.get(&id).unwrap());
    assert_eq!(
        first
            .append_session_for_container_durable(&initial, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let canonical = first.get(&id).unwrap().persistence_identity().sessions[&sid].clone();
    let captured = frozen(&peer.get(&id).unwrap());
    let (release_one, held_one) = hold_store_connection(&peer).await;
    let (release_two, held_two) = hold_store_connection(&peer).await;
    let mut cancelled = Box::pin(peer.append_session_for_container_durable(&captured, &sid));
    assert!(futures::poll!(cancelled.as_mut()).is_pending());
    drop(cancelled);
    let provisional = peer.get(&id).unwrap().persistence_identity().sessions[&sid].clone();
    assert_ne!(provisional, canonical);
    release_one.send(()).unwrap();
    release_two.send(()).unwrap();
    held_one.await.unwrap();
    held_two.await.unwrap();
    assert!(peer.wait_drained(Duration::from_secs(5)).await);
    let captured_provisional = frozen(&peer.get(&id).unwrap());
    assert_eq!(
        captured_provisional
            .persistence_identity()
            .sessions
            .get(&sid),
        Some(&provisional)
    );
    CancelledLookupFixture {
        first,
        peer,
        id,
        sid,
        canonical,
        initial: captured,
        captured: captured_provisional,
    }
}

#[tokio::test]
async fn deferred_canonical_rebind_confirms_only_corrected_current_membership() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let CancelledLookupFixture {
        first,
        peer,
        id,
        sid,
        canonical,
        captured,
        ..
    } = cancelled_lookup_fixture(&dsn).await;
    // SQL already owns the canonical SID, but this captured UUID is not proof.
    assert!(
        !peer
            .is_session_registration_committed(&captured, &sid)
            .await
            .unwrap()
    );
    let (release, held) = hold_advisory_lock(&dsn, format!("project:{id}")).await;
    let result = peer
        .append_session_for_container_durable(&captured, &sid)
        .await
        .unwrap();
    assert!(matches!(result, PersistenceWriteOutcome::Deferred { .. }));
    let current = peer.get(&id).unwrap();
    assert_eq!(
        current.persistence_identity().sessions.get(&sid),
        Some(&canonical)
    );
    assert_eq!(current.latest_session(), Some(sid.as_str()));
    assert!(
        peer.is_session_registration_committed(&captured, &sid)
            .await
            .unwrap(),
        "Deferred readback must recognize SQL routing already merged into the current execution"
    );
    release.send(()).unwrap();
    held.await.unwrap();
    assert!(peer.wait_drained(Duration::from_secs(5)).await);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn sql_retired_captured_generation_cannot_rebind_to_a_live_canonical_sid() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let CancelledLookupFixture {
        first,
        peer,
        sid,
        canonical,
        captured,
        ..
    } = cancelled_lookup_fixture(&dsn).await;
    let retired = captured.persistence_identity().sessions[&sid].clone();
    let outcomes = super::super::writer::execute_registered(
        &first.database,
        vec![super::super::persist_ops::PersistOp::RemoveSession {
            session_id: sid.clone(),
            generation: retired,
        }],
    )
    .await
    .unwrap();
    assert_eq!(
        outcomes, 1,
        "retiring a provisional UUID leaves the canonical row untouched"
    );
    assert_eq!(
        peer.append_session_for_container_durable(&captured, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    assert!(
        !peer
            .is_session_registration_committed(&captured, &sid)
            .await
            .unwrap()
    );
    let reader = store(&dsn).await;
    let original = reader
        .get_by_session_id_with_fetch_result(&sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        original.persistence_identity().sessions.get(&sid),
        Some(&canonical)
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn captured_without_sid_cannot_confirm_past_a_later_accepted_stop() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let CancelledLookupFixture {
        first,
        peer,
        id,
        sid,
        canonical,
        initial,
        captured,
    } = cancelled_lookup_fixture(&dsn).await;
    let (release, held) = hold_advisory_lock(&dsn, format!("project:{id}")).await;
    let mut stop = Box::pin(peer.clear_session_durable(&id));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(futures::poll!(stop.as_mut()).is_pending());
            if peer.get(&id).unwrap().persistence_identity().generation
                != initial.persistence_identity().generation
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let current = peer.get(&id).unwrap();
    assert!(!current.sessions().contains(&sid));
    assert_eq!(
        current.persistence_identity().retired_sessions.get(&sid),
        captured.persistence_identity().sessions.get(&sid)
    );
    assert_ne!(
        current.persistence_identity().retired_sessions.get(&sid),
        Some(&canonical)
    );
    assert!(
        !peer
            .is_session_registration_committed(&initial, &sid)
            .await
            .unwrap(),
        "an empty captured SID list must not bypass a later local Stop while SQL still contains another generation"
    );
    release.send(()).unwrap();
    held.await.unwrap();
    stop.await;
    assert!(peer.wait_drained(Duration::from_secs(5)).await);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn retirement_after_canonical_lookup_fences_the_final_append_transaction() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let CancelledLookupFixture {
        first,
        peer,
        id,
        sid,
        canonical,
        captured,
        ..
    } = cancelled_lookup_fixture(&dsn).await;
    let retired = captured.persistence_identity().sessions[&sid].clone();
    let (release, held) = hold_advisory_lock(&dsn, format!("project:{id}")).await;
    let mut append = Box::pin(peer.append_session_for_container_durable(&captured, &sid));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(futures::poll!(append.as_mut()).is_pending());
            if peer
                .get(&id)
                .unwrap()
                .persistence_identity()
                .sessions
                .get(&sid)
                == Some(&canonical)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // RemoveSession holds only the SID lock. The completion has passed its
    // canonical read but cannot acquire the project lock for its final write.
    assert_eq!(
        super::super::writer::execute_registered(
            &first.database,
            vec![super::super::persist_ops::PersistOp::RemoveSession {
                session_id: sid.clone(),
                generation: retired
            },]
        )
        .await
        .unwrap(),
        1
    );
    release.send(()).unwrap();
    held.await.unwrap();
    assert_eq!(
        append.await.unwrap(),
        PersistenceWriteOutcome::Superseded,
        "a canonical rebind cannot erase the captured UUID's intervening retirement fence"
    );
    assert!(
        !peer
            .is_session_registration_committed(&captured, &sid)
            .await
            .unwrap()
    );
    assert!(peer.wait_drained(Duration::from_secs(5)).await);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn cancelled_original_fallback_cannot_resurrect_after_canonical_rebind_and_stop() {
    delayed_original_fallback_after_stop(false).await;
}

#[tokio::test]
async fn cancelled_original_fallback_cannot_resurrect_after_another_replica_stops_canonical_sid() {
    delayed_original_fallback_after_stop(true).await;
}

async fn delayed_original_fallback_after_stop(remote_stop: bool) {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let CancelledLookupFixture {
        first,
        peer,
        id,
        sid,
        canonical,
        initial,
        captured,
    } = cancelled_lookup_fixture(&dsn).await;
    // Keep the production immutable command at its registered-before-dispatch
    // boundary. Drop queues the original command exactly as cancellation does.
    let pending = {
        let _registration = peer.registration.lock().unwrap();
        let append = super::super::persist_ops::ContainerSessionAppend::from_registration(
            &initial, &captured, &sid,
        )
        .unwrap();
        peer.register_durable(vec![
            super::super::persist_ops::PersistOp::AppendSessionForContainer(Box::new(append)),
        ])
    };
    assert_eq!(
        peer.append_session_for_container_durable(&captured, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(
        peer.get(&id)
            .unwrap()
            .persistence_identity()
            .sessions
            .get(&sid),
        Some(&canonical)
    );
    if remote_stop {
        let stop_replica = store(&dsn).await;
        stop_replica.clear_session_durable(&id).await;
        assert!(
            stop_replica
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
    } else {
        peer.clear_session_durable(&id).await;
    }
    drop(pending);
    assert!(peer.wait_drained(Duration::from_secs(5)).await);
    let reader = store(&dsn).await;
    let resurrected = reader
        .get_by_session_id_with_fetch_result(&sid)
        .await
        .unwrap()
        .is_some();
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        !resurrected,
        "a cancelled original UUID must not revive the SID after the canonical UUID was stopped"
    );
}

#[tokio::test]
async fn explicit_session_readmission_keeps_a_reused_sid_completable() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("explicit-session-readmission-{suffix}");
    let sid = format!("explicit-session-readmission-sid-{suffix}");
    let physical = basic(&id);
    let writer = store(&dsn).await;
    writer
        .insert_with_session_durable(id.clone(), project(&id, &id, &physical), &sid)
        .await
        .unwrap();
    let retired = writer.get(&id).unwrap().persistence_identity().sessions[&sid].clone();
    writer.clear_session_durable(&id).await;
    let next = writer.get(&id).unwrap();
    writer
        .insert_with_session_durable(id.clone(), next, &sid)
        .await
        .unwrap();
    let captured = frozen(&writer.get(&id).unwrap());
    let readmitted = captured.persistence_identity().sessions[&sid].clone();
    assert_ne!(retired, readmitted);
    assert_eq!(
        writer
            .append_session_for_container_durable(&captured, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed,
        "a prior retirement cannot reject an existing explicitly readmitted SID incarnation"
    );
    assert!(
        writer
            .is_session_registration_committed(&captured, &sid)
            .await
            .unwrap()
    );
    let reader = store(&dsn).await;
    let persisted = reader
        .get_by_session_id_with_fetch_result(&sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        persisted.persistence_identity().sessions.get(&sid),
        Some(&readmitted)
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn cancelled_lookup_converges_through_fresh_metadata_admission_before_dispatch() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let CancelledLookupFixture {
        first,
        peer,
        id,
        sid,
        canonical,
        ..
    } = cancelled_lookup_fixture(&dsn).await;
    let request_id = format!("fresh-metadata-{id}");
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        // Match the fresh engine loop: hold incoming request fields, merge onto
        // the current registration, and refresh on a pre-dispatch rejection.
        let mut incoming = (*peer.get(&id).unwrap()).clone();
        incoming.set_request_id(Some(request_id.clone()));
        incoming.set_model_provider(Some(shared_types::ModelProviderConfig {
            id: "fresh-metadata-model".into(),
            name: "fixture".into(),
            base_url: "https://model.invalid".into(),
            api_key: "test-only-not-a-secret".into(),
            default_model: "fixture".into(),
            requires_openai_auth: false,
            api_protocol: None,
            wire_api: None,
        }));
        let outcome = peer
            .insert_project_registration_durable(id.clone(), Arc::new(incoming))
            .await
            .unwrap();
        outcomes.push(outcome.clone());
        if outcome == PersistenceWriteOutcome::Committed {
            break;
        }
        assert_eq!(outcome, PersistenceWriteOutcome::Superseded);
        peer.refresh_project_registration(&id)
            .await
            .unwrap()
            .unwrap();
    }
    let committed = outcomes.last() == Some(&PersistenceWriteOutcome::Committed);
    let current = frozen(&peer.get(&id).unwrap());
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        committed,
        "fresh ordinary metadata must converge without hand-registering the SID: {outcomes:?}"
    );
    assert_eq!(
        current.persistence_identity().sessions.get(&sid),
        Some(&canonical)
    );
    assert_eq!(current.latest_session(), Some(sid.as_str()));
    assert_eq!(current.request_id(), Some(request_id.as_str()));
    assert_eq!(current.model_provider().unwrap().id, "fresh-metadata-model");
}
