//! A known CAS rejection must not poison the next undispatched fresh request.
use super::cross_replica_existing_session::{hold_store_connection, single_connection_peer};
use super::*;

#[tokio::test]
async fn superseded_container_intent_preserves_a_newer_intent_and_both_fresh_projects_recover() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let a = format!("rejected-cursor-a-{suffix}");
    let b = format!("rejected-cursor-b-{suffix}");
    let owner = format!("rejected-cursor-owner-{suffix}");
    let physical = basic(&owner);
    let first = store(&dsn).await;
    first
        .insert_with_session_durable(
            a.clone(),
            project(&a, &owner, &physical),
            &format!("a-original-{suffix}"),
        )
        .await
        .unwrap();
    first
        .insert_with_session_durable(
            b.clone(),
            project(&b, &owner, &physical),
            &format!("b-original-{suffix}"),
        )
        .await
        .unwrap();
    let peer = single_connection_peer(&dsn).await;
    let key = crate::adapter::container_entry_key(&peer.get(&a).unwrap());
    let canonical = peer.container_registration.lock().unwrap()[&key].clone();
    let frame = first
        .capture_project_stop_registration(&a)
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
    let (release, held) = hold_store_connection(&peer).await;
    let mut request_a = (*peer.get(&a).unwrap()).clone();
    request_a.set_request_id(Some("rejected-a".into()));
    let mut append_a =
        Box::pin(peer.insert_project_registration_durable(a.clone(), Arc::new(request_a)));
    assert!(futures::poll!(append_a.as_mut()).is_pending());
    let mut request_b = (*peer.get(&b).unwrap()).clone();
    request_b.set_request_id(Some("rejected-b".into()));
    let mut append_b =
        Box::pin(peer.insert_project_registration_durable(b.clone(), Arc::new(request_b)));
    assert!(futures::poll!(append_b.as_mut()).is_pending());
    let newer = peer.container_registration.lock().unwrap()[&key].clone();
    assert_eq!(newer.revision, canonical.revision + 2);
    release.send(()).unwrap();
    held.await.unwrap();
    assert_eq!(append_a.await.unwrap(), PersistenceWriteOutcome::Superseded);
    assert_eq!(
        peer.container_registration.lock().unwrap()[&key],
        newer,
        "A's rejection must not roll back B's still-unconfirmed newer cursor"
    );
    assert_eq!(append_b.await.unwrap(), PersistenceWriteOutcome::Superseded);
    assert_eq!(
        peer.container_registration.lock().unwrap()[&key],
        canonical,
        "B's confirmed rejection must restore SQL's cursor, not A's rejected previous value"
    );
    assert!(peer.rejected_container_intents.lock().unwrap().is_empty());
    let mut next_b = (*peer.get(&b).unwrap()).clone();
    next_b.set_request_id(Some("fresh-b-after-known-rejection".into()));
    assert_eq!(
        peer.insert_project_registration_durable(b.clone(), Arc::new(next_b))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed,
        "the original fresh metadata entrance must recover its rejected project projection too"
    );
    let new_a = peer
        .refresh_project_registration(&a)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        new_a.persistence_identity().generation,
        frame.persistence_identity().generation
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn rejected_metadata_keeps_an_accepted_uncommitted_sid_from_before_its_capture() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let a = format!("pending-routing-a-{suffix}");
    let b = format!("pending-routing-b-{suffix}");
    let owner = format!("pending-routing-owner-{suffix}");
    let sid = format!("accepted-pending-{suffix}");
    let physical = basic(&owner);
    let first = store(&dsn).await;
    first
        .insert_project_registration_durable(a.clone(), project(&a, &owner, &physical))
        .await
        .unwrap();
    first
        .insert_project_registration_durable(b.clone(), project(&b, &owner, &physical))
        .await
        .unwrap();
    let peer = single_connection_peer(&dsn).await;
    let old_a = first
        .capture_project_stop_registration(&a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first
            .clear_sessions_if_current_durable(&old_a)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let (release, held) = hold_store_connection(&peer).await;
    let mut invalid_a = (*peer.get(&a).unwrap()).clone();
    invalid_a.set_request_id(Some("rejected-a".into()));
    let mut write_a =
        Box::pin(peer.insert_project_registration_durable(a.clone(), Arc::new(invalid_a)));
    assert!(futures::poll!(write_a.as_mut()).is_pending());
    let accepted = frozen(&peer.get(&b).unwrap());
    let mut append = Box::pin(peer.append_session_for_container_durable(&accepted, &sid));
    assert!(futures::poll!(append.as_mut()).is_pending());
    let pending_generation = peer.get(&b).unwrap().persistence_identity().sessions[&sid].clone();
    let mut rejected_b = (*peer.get(&b).unwrap()).clone();
    rejected_b.set_request_id(Some("rejected-b".into()));
    let mut write_b =
        Box::pin(peer.insert_project_registration_durable(b.clone(), Arc::new(rejected_b)));
    assert!(futures::poll!(write_b.as_mut()).is_pending());
    release.send(()).unwrap();
    held.await.unwrap();
    assert_eq!(write_a.await.unwrap(), PersistenceWriteOutcome::Superseded);
    assert_eq!(write_b.await.unwrap(), PersistenceWriteOutcome::Superseded);
    let retained = peer.get(&b).unwrap();
    assert!(
        retained.sessions().contains(&sid),
        "metadata rollback cannot retract an earlier accepted SID"
    );
    assert_eq!(
        retained.persistence_identity().sessions.get(&sid),
        Some(&pending_generation)
    );
    assert_eq!(retained.latest_session(), Some(sid.as_str()));
    assert!(
        !retained
            .persistence_identity()
            .retired_sessions
            .contains_key(&sid)
    );
    assert!(peer.get_by_session_id(&sid).is_some());
    assert_eq!(append.await.unwrap(), PersistenceWriteOutcome::Committed);
    let mut fresh = (*peer.get(&b).unwrap()).clone();
    fresh.set_request_id(Some("fresh-after-rejected-metadata".into()));
    assert_eq!(
        peer.insert_project_registration_durable(b.clone(), Arc::new(fresh))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

struct FaultedCursor {
    first: PgStore,
    peer: PgStore,
    id: String,
    physical: ContainerBasicInfo,
    frame: Arc<ProjectAndContainerInfo>,
}

async fn faulted_cursor(dsn: &str) -> FaultedCursor {
    use sha2::Digest;
    let suffix = test_support::uuid_suffix();
    let id = format!("rejected-readback-timeout-{suffix}");
    let physical = basic(&id);
    let first = store(dsn).await;
    first
        .insert_project_registration_durable(id.clone(), project(&id, &id, &physical))
        .await
        .unwrap();
    let peer = single_connection_peer(dsn).await;
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
    let (release, held) = hold_store_connection(&peer).await;
    let mut next = (*peer.get(&id).unwrap()).clone();
    next.set_request_id(Some("known-rejected-before-read-fault".into()));
    let mut write = Box::pin(peer.insert_project_registration_durable(id.clone(), Arc::new(next)));
    assert!(futures::poll!(write.as_mut()).is_pending());
    let captured = peer.get(&id).unwrap();
    let op = crate::pg::project_store::persist_ops::registration_for_metadata(&captured).unwrap();
    let crate::pg::project_store::persist_ops::PersistOp::RegisterProject {
        container,
        project,
        ..
    } = op
    else {
        panic!("typed registration");
    };
    let fingerprint = sha2::Sha256::digest(serde_json::to_vec(&(container, project)).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    release.send(()).unwrap();
    held.await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let fingerprint = fingerprint.clone();
            let rejected = crate::pg::project_store::database::read(&first.database, move |tx| Box::pin(async move {
                Ok(!toasty::sql::query("SELECT request_id FROM project_write_receipts WHERE fingerprint=$1 AND outcome='superseded'").bind(fingerprint).exec(tx).await?.is_empty())
            })).await.unwrap();
            if rejected { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    // The mutation's real SQL Superseded receipt is committed. Block only the
    // subsequent authoritative cursor read, never invent an unknown write.
    let (release_read, blocked_read) = hold_store_connection(&peer).await;
    assert_eq!(write.await.unwrap(), PersistenceWriteOutcome::Superseded);
    assert!(!peer.rejected_container_intents.lock().unwrap().is_empty());
    release_read.send(()).unwrap();
    blocked_read.await.unwrap();
    FaultedCursor {
        first,
        peer,
        id,
        physical,
        frame,
    }
}

#[tokio::test]
async fn known_superseded_cursor_readback_timeout_recovers_on_the_original_fresh_entry() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let FaultedCursor {
        first,
        peer,
        id,
        frame,
        ..
    } = faulted_cursor(&dsn).await;
    let recovered = peer
        .refresh_project_registration(&id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        recovered.persistence_identity().generation,
        frame.persistence_identity().generation
    );
    assert!(peer.rejected_container_intents.lock().unwrap().is_empty());
    let mut fresh = (*recovered).clone();
    fresh.set_request_id(Some("fresh-after-read-fault".into()));
    assert_eq!(
        peer.insert_project_registration_durable(id.clone(), Arc::new(fresh))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn obsolete_rejected_cursor_drops_only_its_marker_before_verified_physical_recovery() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let FaultedCursor {
        first,
        peer,
        id,
        physical,
        frame,
    } = faulted_cursor(&dsn).await;
    let proof = evidence(&physical);
    let canonical_before = frozen(&first.get(&id).unwrap());
    assert_eq!(
        first
            .replace_agent_container_registration_durable(&canonical_before, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let key = crate::adapter::container_entry_key(&peer.get(&id).unwrap());
    let unchanged = peer.container_registration.lock().unwrap()[&key].clone();
    assert!(
        peer.reconcile_rejected_container_intent(
            &key,
            tokio::time::Instant::now() + Duration::from_millis(600)
        )
        .await
        .is_err()
    );
    assert!(peer.rejected_container_intents.lock().unwrap().is_empty());
    assert_eq!(
        peer.container_registration.lock().unwrap()[&key],
        unchanged,
        "expiry is not permission to adopt a changed physical runtime"
    );
    assert_eq!(
        peer.replace_agent_container_registration_durable(&frame, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed,
        "the existing verified fresh recovery must not be blocked by an obsolete rejection marker"
    );
    assert_eq!(
        peer.get(&id)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        proof.replacement.container_id
    );
    let mut fresh = (*peer.get(&id).unwrap()).clone();
    fresh.set_request_id(Some("fresh-after-verified-replacement".into()));
    assert_eq!(
        peer.insert_project_registration_durable(id.clone(), Arc::new(fresh))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn removed_container_expires_rejected_cursor_without_resurrecting_old_registration() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let FaultedCursor {
        first,
        peer,
        id,
        physical,
        frame,
    } = faulted_cursor(&dsn).await;
    assert!(
        first
            .delete_container_with_projects_durable(&physical.container_id)
            .await
            .0
    );
    let key = crate::adapter::container_entry_key(&peer.get(&id).unwrap());
    let unchanged = peer.container_registration.lock().unwrap()[&key].clone();
    assert!(
        peer.reconcile_rejected_container_intent(
            &key,
            tokio::time::Instant::now() + Duration::from_millis(600)
        )
        .await
        .is_err()
    );
    assert!(peer.rejected_container_intents.lock().unwrap().is_empty());
    assert_eq!(peer.container_registration.lock().unwrap()[&key], unchanged);
    assert!(
        peer.refresh_project_registration(&id)
            .await
            .unwrap()
            .is_none(),
        "normal authoritative absence is no longer blocked by the expired marker"
    );
    assert_eq!(
        peer.append_session_for_container_durable(&frame, "late-retired-runtime")
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}
