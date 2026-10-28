//! An ordinary runner can return a SID committed after another replica loaded
//! the same project revision. That cache miss does not create a new execution.
use super::*;

#[tokio::test]
async fn existing_session_completion_on_unsynced_replica_keeps_committed_generation() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("unsynced-existing-project-{suffix}");
    let owner = format!("unsynced-existing-owner-{suffix}");
    let sid = format!("unsynced-existing-session-{suffix}");
    let physical = basic(&owner);
    let first = store(&dsn).await;
    assert_eq!(
        first
            .insert_project_registration_durable(id.clone(), project(&id, &owner, &physical))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );

    // PgStore::connect loads real SQL state. No mirror-sync loop is started by
    // this storage fixture, so the peer stays deterministically before A's SID.
    let peer = store(&dsn).await;
    let first_before = frozen(&first.get(&id).unwrap());
    let peer_before = frozen(&peer.get(&id).unwrap());
    assert_eq!(
        first_before.persistence_identity().revision,
        peer_before.persistence_identity().revision
    );
    assert_eq!(
        first_before.persistence_identity().generation,
        peer_before.persistence_identity().generation
    );
    assert!(peer_before.sessions().is_empty());

    // A completes its first admitted request. Session append intentionally
    // does not advance the project's structural metadata revision.
    assert_eq!(
        first
            .append_session_for_container_durable(&first_before, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let first_completed = first.get(&id).unwrap();
    let committed_sid_generation = first_completed.persistence_identity().sessions[&sid].clone();
    assert_eq!(
        first_before.persistence_identity().revision,
        first_completed.persistence_identity().revision
    );
    assert!(!peer.get(&id).unwrap().sessions().contains(&sid));

    // B admits a newer chat using its legitimately cached same-revision view.
    // Preserve this request's metadata while joining any committed SID routing.
    let request_id = format!("newer-request-{suffix}");
    let mut newer = (*peer.get(&id).unwrap()).clone();
    newer.set_request_id(Some(request_id.clone()));
    newer.set_model_provider(Some(shared_types::ModelProviderConfig {
        id: "newer-model".into(),
        name: "protocol-fixture".into(),
        base_url: "https://model.invalid".into(),
        api_key: "test-only-not-a-secret".into(),
        default_model: "fixture".into(),
        requires_openai_auth: false,
        api_protocol: None,
        wire_api: None,
    }));
    assert_eq!(
        peer.insert_project_registration_durable(id.clone(), Arc::new(newer))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let captured = frozen(&peer.get(&id).unwrap());
    let lookup_sid = sid.clone();
    let (persisted_before, container_before) =
        crate::pg::project_store::database::read(&first.database, move |tx| {
            Box::pin(async move {
                crate::pg::project_store::repo::fetch_project_by_session(tx, &lookup_sid).await
            })
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        persisted_before.session_identities.get(&sid),
        Some(&committed_sid_generation),
        "B's newer metadata cannot retire A's already committed SID"
    );
    assert_eq!(
        persisted_before.latest_session.as_deref(),
        Some(sid.as_str()),
        "metadata admission must preserve the already committed selection"
    );
    assert_eq!(
        persisted_before.generation,
        captured.persistence_identity().generation
    );
    let persisted_physical = container_before.unwrap();
    assert_eq!(
        persisted_physical.container_id.as_deref(),
        Some(physical.container_id.as_str())
    );
    assert_eq!(persisted_physical.workload_uid, physical.workload_uid);
    assert_eq!(
        Some(&persisted_physical.container_generation),
        captured
            .persistence_identity()
            .container
            .as_ref()
            .map(|identity| &identity.generation)
    );
    eprintln!(
        "before reused-SID completion: cached_member={}, cached_sid_generation={:?}, sql_latest={:?}, sql_sid_generation={}, project_revision={}",
        captured.sessions().contains(&sid),
        captured.persistence_identity().sessions.get(&sid),
        persisted_before.latest_session,
        committed_sid_generation,
        persisted_before.row_revision
    );

    // This represents the original runner's response identity only. No prompt,
    // replacement, stop, session clear or synthetic model response is involved.
    let outcome = peer
        .append_session_for_container_durable(&captured, &sid)
        .await
        .unwrap();
    let local_after = peer.get(&id).unwrap();
    let readback = peer
        .is_session_registration_committed(&captured, &sid)
        .await
        .unwrap();
    let reader = store(&dsn).await;
    let authoritative = reader
        .get_by_session_id_with_fetch_result(&sid)
        .await
        .unwrap()
        .unwrap();
    eprintln!(
        "after reused-SID completion: outcome={outcome:?}, authoritative_readback={readback}, local_member={}, local_latest={:?}, sql_latest={:?}, same_pod={}, same_workload={}",
        local_after.sessions().contains(&sid),
        local_after.latest_session(),
        authoritative.latest_session(),
        authoritative.container_info().unwrap().container_id == physical.container_id,
        authoritative.container_info().unwrap().workload_uid == physical.workload_uid
    );

    // Close only the fixture's database owners, even when the final regression
    // assertion below fails. The disposable PostgreSQL service remains running.
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);

    assert_eq!(
        outcome,
        PersistenceWriteOutcome::Committed,
        "a reused committed SID on the same project/Pod/workload is a cache miss, not a replaced execution"
    );
    assert!(readback);
    assert!(local_after.sessions().contains(&sid));
    assert_eq!(local_after.latest_session(), Some(sid.as_str()));
    assert_eq!(
        local_after.persistence_identity().sessions.get(&sid),
        Some(&committed_sid_generation)
    );
    assert_eq!(local_after.request_id(), Some(request_id.as_str()));
    assert_eq!(local_after.model_provider().unwrap().id, "newer-model");
    assert_eq!(authoritative.latest_session(), Some(sid.as_str()));
    assert_eq!(
        authoritative.persistence_identity().sessions.get(&sid),
        Some(&committed_sid_generation)
    );
    assert_eq!(authoritative.request_id(), Some(request_id.as_str()));
    assert_eq!(authoritative.model_provider().unwrap().id, "newer-model");
    assert_eq!(
        authoritative.container_info().unwrap().container_id,
        physical.container_id
    );
    assert_eq!(
        authoritative.container_info().unwrap().workload_uid,
        physical.workload_uid
    );
}

#[tokio::test]
async fn missing_local_membership_cannot_adopt_a_foreign_projects_sid() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("foreign-sid-owner-{suffix}");
    let owned = format!("foreign-sid-target-{suffix}");
    let foreign = format!("foreign-sid-original-{suffix}");
    let sid = format!("foreign-sid-{suffix}");
    let physical = basic(&owner);
    let writer = store(&dsn).await;
    assert_eq!(
        writer
            .insert_project_registration_durable(owned.clone(), project(&owned, &owner, &physical),)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let stale = store(&dsn).await;
    writer
        .insert_with_session_durable(foreign.clone(), project(&foreign, &owner, &physical), &sid)
        .await
        .unwrap();
    let captured = frozen(&stale.get(&owned).unwrap());
    let original_generation = writer
        .get(&foreign)
        .unwrap()
        .persistence_identity()
        .sessions[&sid]
        .clone();
    let outcome = stale
        .append_session_for_container_durable(&captured, &sid)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        PersistenceWriteOutcome::Superseded,
        "a SID already belonging to another project must never be adopted"
    );
    assert!(!stale.get(&owned).unwrap().sessions().contains(&sid));
    let reader = store(&dsn).await;
    let original = reader
        .get_by_session_id_with_fetch_result(&sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.project_id(), foreign);
    assert_eq!(
        original.persistence_identity().sessions.get(&sid),
        Some(&original_generation)
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(stale.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn retired_committed_sid_cannot_be_restored_by_a_stale_replica_completion() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("retired-existing-project-{suffix}");
    let sid = format!("retired-existing-session-{suffix}");
    let physical = basic(&id);
    let writer = store(&dsn).await;
    writer
        .insert_with_session_durable(id.clone(), project(&id, &id, &physical), &sid)
        .await
        .unwrap();
    let stale = store(&dsn).await;
    let captured = frozen(&stale.get(&id).unwrap());
    assert!(writer.clear_session_one_durable(&id, &sid).await);
    assert_eq!(
        stale
            .append_session_for_container_durable(&captured, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    let reader = store(&dsn).await;
    assert!(
        reader
            .get_by_session_id_with_fetch_result(&sid)
            .await
            .unwrap()
            .is_none()
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(stale.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn canonical_sid_lookup_cannot_rebind_an_old_physical_execution() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("replaced-existing-project-{suffix}");
    let sid = format!("replaced-existing-session-{suffix}");
    let physical = basic(&id);
    let writer = store(&dsn).await;
    writer
        .insert_with_session_durable(id.clone(), project(&id, &id, &physical), &sid)
        .await
        .unwrap();
    let captured = frozen(&writer.get(&id).unwrap());
    let replacement = evidence(&physical);
    assert_eq!(
        writer
            .replace_agent_container_registration_durable(&captured, &replacement)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(
        writer
            .append_session_for_container_durable(&captured, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    assert_eq!(
        writer
            .get(&id)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        replacement.replacement.container_id
    );
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn old_completion_without_request_id_preserves_newer_metadata_and_latest() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("none-request-project-{suffix}");
    let first_sid = format!("none-request-first-{suffix}");
    let newer_sid = format!("none-request-newer-{suffix}");
    let completed_sid = format!("none-request-completed-{suffix}");
    let physical = basic(&id);
    let writer = store(&dsn).await;
    writer
        .insert_with_session_durable(id.clone(), project(&id, &id, &physical), &first_sid)
        .await
        .unwrap();
    let captured = frozen(&writer.get(&id).unwrap());
    assert!(captured.request_id().is_none());
    let mut newer = (*writer.get(&id).unwrap()).clone();
    newer.set_model_provider(Some(shared_types::ModelProviderConfig {
        id: "newer-without-request-id".into(),
        name: "protocol-fixture".into(),
        base_url: "https://model.invalid".into(),
        api_key: "test-only-not-a-secret".into(),
        default_model: "fixture".into(),
        requires_openai_auth: false,
        api_protocol: None,
        wire_api: None,
    }));
    writer
        .insert_with_session_durable(id.clone(), Arc::new(newer), &newer_sid)
        .await
        .unwrap();
    assert_eq!(
        writer
            .append_session_for_container_durable(&captured, &completed_sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed,
        "newer metadata does not invalidate membership of the old execution"
    );
    let current = writer.get(&id).unwrap();
    assert!(current.request_id().is_none());
    assert!(current.sessions().contains(&completed_sid));
    assert_eq!(current.latest_session(), Some(newer_sid.as_str()));
    assert_eq!(
        current.model_provider().unwrap().id,
        "newer-without-request-id"
    );
    let reader = store(&dsn).await;
    let persisted = reader
        .get_by_session_id_with_fetch_result(&completed_sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.latest_session(), Some(newer_sid.as_str()));
    assert_eq!(
        persisted.model_provider().unwrap().id,
        "newer-without-request-id"
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

pub(super) async fn single_connection_peer(dsn: &str) -> PgStore {
    PgStore::connect(
        &PostgresConfig {
            url: Some(dsn.to_owned()),
            max_connections: Some(1),
            min_connections: Some(1),
            // 钉住 600ms 预算：本文件多个测试断言"共享 deadline"语义，
            // 与运行期默认值(15000ms)解耦，避免默认值调整改变这些用例的时序。
            durable_commit_timeout_ms: Some(600),
            ..Default::default()
        },
        "isolated-test".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap()
}

pub(super) async fn hold_store_connection(
    store: &PgStore,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let database = store.database.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        database
            .execute(move |mut db| async move {
                let mut tx = db.transaction().await?;
                toasty::sql::query("SELECT 1").exec(&mut tx).await?;
                started.send(()).unwrap();
                wait.await.unwrap();
                tx.rollback().await?;
                Ok(())
            })
            .await
            .unwrap();
    });
    ready.await.unwrap();
    (release, task)
}

#[tokio::test]
async fn cancelled_canonical_lookup_keeps_fallback_and_next_append_can_recover() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("cancelled-lookup-project-{suffix}");
    let sid = format!("cancelled-lookup-session-{suffix}");
    let physical = basic(&id);
    let first = store(&dsn).await;
    first
        .insert_project_registration_durable(id.clone(), project(&id, &id, &physical))
        .await
        .unwrap();
    let peer = Arc::new(single_connection_peer(&dsn).await);
    let initial = frozen(&first.get(&id).unwrap());
    assert_eq!(
        first
            .append_session_for_container_durable(&initial, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let canonical_generation =
        first.get(&id).unwrap().persistence_identity().sessions[&sid].clone();
    let captured = frozen(&peer.get(&id).unwrap());
    let (release, held) = hold_store_connection(&peer).await;
    let task_peer = Arc::clone(&peer);
    let task_expected = captured.clone();
    let task_sid = sid.clone();
    let task = tokio::spawn(async move {
        task_peer
            .append_session_for_container_durable(&task_expected, &task_sid)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while peer.pending_ops.load(std::sync::atomic::Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(peer.get(&id).unwrap().sessions().contains(&sid));
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let accepted = peer.get(&id).unwrap();
    assert!(accepted.sessions().contains(&sid));
    assert!(
        !accepted
            .persistence_identity()
            .retired_sessions
            .contains_key(&sid)
    );
    release.send(()).unwrap();
    held.await.unwrap();
    assert!(peer.wait_drained(Duration::from_secs(5)).await);
    assert_eq!(
        peer.append_session_for_container_durable(&captured, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed,
        "retry registration with the accepted SID, never resend the prompt"
    );
    assert_eq!(
        peer.get(&id)
            .unwrap()
            .persistence_identity()
            .sessions
            .get(&sid),
        Some(&canonical_generation)
    );
    assert_eq!(peer.get(&id).unwrap().latest_session(), Some(sid.as_str()));
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn canonical_lookup_and_append_share_one_durable_deadline() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("lookup-deadline-project-{suffix}");
    let sid = format!("lookup-deadline-session-{suffix}");
    let physical = basic(&id);
    let first = store(&dsn).await;
    first
        .insert_project_registration_durable(id.clone(), project(&id, &id, &physical))
        .await
        .unwrap();
    let peer = single_connection_peer(&dsn).await;
    let initial = frozen(&first.get(&id).unwrap());
    assert_eq!(
        first
            .append_session_for_container_durable(&initial, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let canonical_generation =
        first.get(&id).unwrap().persistence_identity().sessions[&sid].clone();
    let captured = frozen(&peer.get(&id).unwrap());
    let (release_connection, connection) = hold_store_connection(&peer).await;
    let (release_write, write_lock) = hold_advisory_lock(&dsn, format!("project:{id}")).await;
    let release_read = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        release_connection.send(()).unwrap();
        connection.await.unwrap();
    });
    let started = tokio::time::Instant::now();
    let outcome = peer
        .append_session_for_container_durable(&captured, &sid)
        .await
        .unwrap();
    let elapsed = started.elapsed();
    release_write.send(()).unwrap();
    write_lock.await.unwrap();
    release_read.await.unwrap();
    assert!(matches!(outcome, PersistenceWriteOutcome::Deferred { .. }));
    assert!(
        elapsed < Duration::from_millis(850),
        "lookup and append must share 600ms, rather than adding a new budget: {elapsed:?}"
    );
    assert!(
        peer.get(&id).unwrap().sessions().contains(&sid),
        "a canonical generation already proven committed is not provisional"
    );
    assert!(peer.wait_drained(Duration::from_secs(5)).await);
    assert_eq!(
        peer.get(&id)
            .unwrap()
            .persistence_identity()
            .sessions
            .get(&sid),
        Some(&canonical_generation)
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn older_lookup_drop_preserves_later_committed_same_generation() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("confirmed-before-old-drop-{suffix}");
    let sid = format!("confirmed-before-old-drop-sid-{suffix}");
    let physical = basic(&id);
    let peer = single_connection_peer(&dsn).await;
    peer.insert_project_registration_durable(id.clone(), project(&id, &id, &physical))
        .await
        .unwrap();
    let captured = frozen(&peer.get(&id).unwrap());
    let (release, connection) = hold_store_connection(&peer).await;
    let mut older = Box::pin(peer.append_session_for_container_durable(&captured, &sid));
    assert!(futures::poll!(older.as_mut()).is_pending());
    let generation = peer.get(&id).unwrap().persistence_identity().sessions[&sid].clone();
    release.send(()).unwrap();
    connection.await.unwrap();
    // A stays deliberately unpolled, even after its read may have completed.
    assert_eq!(
        peer.append_session_for_container_durable(&captured, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    drop(older);
    let after = peer.get(&id).unwrap();
    let member = after.sessions().contains(&sid);
    let latest = after.latest_session().map(str::to_owned);
    assert!(peer.wait_drained(Duration::from_secs(5)).await);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        member,
        "an older lookup cancellation cannot retract B's already committed membership"
    );
    assert_eq!(latest.as_deref(), Some(sid.as_str()));
    assert_eq!(
        after.persistence_identity().sessions.get(&sid),
        Some(&generation)
    );
}

#[tokio::test]
async fn later_commit_preserves_membership_after_older_lookup_drop() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("commit-after-old-drop-{suffix}");
    let sid = format!("commit-after-old-drop-sid-{suffix}");
    let physical = basic(&id);
    let peer = single_connection_peer(&dsn).await;
    peer.insert_project_registration_durable(id.clone(), project(&id, &id, &physical))
        .await
        .unwrap();
    let captured = frozen(&peer.get(&id).unwrap());
    let (release, connection) = hold_store_connection(&peer).await;
    let mut older = Box::pin(peer.append_session_for_container_durable(&captured, &sid));
    assert!(futures::poll!(older.as_mut()).is_pending());
    let generation = peer.get(&id).unwrap().persistence_identity().sessions[&sid].clone();
    let mut later = Box::pin(peer.append_session_for_container_durable(&captured, &sid));
    assert!(futures::poll!(later.as_mut()).is_pending());
    drop(older);
    assert!(peer.get(&id).unwrap().sessions().contains(&sid));
    release.send(()).unwrap();
    connection.await.unwrap();
    assert_eq!(later.await.unwrap(), PersistenceWriteOutcome::Committed);
    let after = peer.get(&id).unwrap();
    let member = after.sessions().contains(&sid);
    let latest = after.latest_session().map(str::to_owned);
    assert!(peer.wait_drained(Duration::from_secs(5)).await);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        member,
        "B must preserve confirmed routing when A's older lookup is cancelled"
    );
    assert_eq!(latest.as_deref(), Some(sid.as_str()));
    assert_eq!(
        after.persistence_identity().sessions.get(&sid),
        Some(&generation)
    );
}

#[tokio::test]
async fn cancelled_lookup_then_accepted_stop_cannot_resurrect_fallback_sid() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("cancelled-lookup-stop-{suffix}");
    let sid = format!("cancelled-lookup-stop-sid-{suffix}");
    let physical = basic(&id);
    let peer = single_connection_peer(&dsn).await;
    peer.insert_project_registration_durable(id.clone(), project(&id, &id, &physical))
        .await
        .unwrap();
    let captured = frozen(&peer.get(&id).unwrap());
    let (release, connection) = hold_store_connection(&peer).await;
    let mut append = Box::pin(peer.append_session_for_container_durable(&captured, &sid));
    assert!(futures::poll!(append.as_mut()).is_pending());
    drop(append);
    let mut stop = Box::pin(peer.clear_session_durable(&id));
    assert!(futures::poll!(stop.as_mut()).is_pending());
    release.send(()).unwrap();
    connection.await.unwrap();
    stop.await;
    assert!(peer.wait_drained(Duration::from_secs(5)).await);
    let reader = store(&dsn).await;
    let resurrected = reader
        .get_by_session_id_with_fetch_result(&sid)
        .await
        .unwrap()
        .is_some();
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        !resurrected,
        "Stop accepted after lookup cancellation must retire the original queued SID generation"
    );
}
