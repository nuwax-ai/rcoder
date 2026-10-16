//! Targeted authoritative reads recover an already-poisoned pre-fix projection.
use super::*;

#[tokio::test]
async fn poisoned_same_workload_member_refreshes_detached_sql_registration_without_sync() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("legacy-pod-owner-{suffix}");
    let a = format!("legacy-pod-a-{suffix}");
    let b = format!("legacy-pod-b-{suffix}");
    let sid_b = format!("legacy-pod-sid-b-{suffix}");
    let original = basic(&owner);
    let writer = store(&dsn).await;
    writer
        .insert_with_session_durable(b.clone(), project(&b, &owner, &original), &sid_b)
        .await
        .unwrap();
    writer
        .insert_project_registration_durable(a.clone(), project(&a, &owner, &original))
        .await
        .unwrap();
    let old_b = frozen(&writer.get(&b).unwrap());
    let mut proof = evidence(&original);
    proof.replacement.workload_uid = original.workload_uid.clone();
    // Reproduce the exact pre-fix ordinary C write: the shared Arc gets P1,
    // while D retains a persistence identity for P0/g0.
    let mut c_update = (*writer.get(&a).unwrap()).clone();
    c_update.set_container(Some(proof.replacement.clone()));
    writer
        .insert_project_registration_durable(a.clone(), Arc::new(c_update))
        .await
        .unwrap();
    let poisoned_b = writer.get(&b).unwrap();
    assert_eq!(
        poisoned_b.container_info().unwrap().container_id,
        proof.replacement.container_id
    );
    assert_eq!(
        poisoned_b
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .physical_uid
            .as_deref(),
        Some(original.container_id.as_str())
    );
    let error = writer
        .insert_project_registration_durable(b.clone(), poisoned_b)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("stale snapshot cannot be rebased")
    );
    let unrelated = writer.register_durable(vec![
        crate::pg::project_store::persist_ops::PersistOp::TouchProject {
            project_id: format!("unrelated-{suffix}"),
            generation: "unrelated-generation".into(),
            last_activity: chrono::Utc::now(),
        },
    ]);
    assert!(
        writer
            .pending_ops
            .load(std::sync::atomic::Ordering::Acquire)
            > 0
    );
    // The new path reads the real detached row, not a fabricated old Pod.
    let detached = writer
        .refresh_project_registration(&b)
        .await
        .unwrap()
        .unwrap();
    assert!(detached.container_info().is_none());
    assert!(detached.persistence_identity().container.is_none());
    assert!(detached.sessions().contains(&sid_b));
    proof.previous = None;
    assert_eq!(
        writer
            .replace_agent_container_registration_durable(&detached, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let reader = store(&dsn).await;
    let resolved = reader
        .get_by_session_id_with_fetch_result(&sid_b)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        resolved.container_info().unwrap().container_id,
        proof.replacement.container_id
    );
    assert_ne!(
        resolved.persistence_identity().generation,
        old_b.persistence_identity().generation
    );
    assert_eq!(
        writer
            .append_session_for_container_durable(&old_b, "legacy-late-completion")
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    drop(unrelated);
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn poisoned_bound_registration_refresh_admits_while_unrelated_work_is_pending() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("bound-refresh-owner-{suffix}");
    let a = format!("bound-refresh-a-{suffix}");
    let b = format!("bound-refresh-b-{suffix}");
    let sid = format!("bound-refresh-sid-{suffix}");
    let original = basic(&owner);
    let writer = store(&dsn).await;
    writer
        .insert_with_session_durable(b.clone(), project(&b, &owner, &original), &sid)
        .await
        .unwrap();
    writer
        .insert_project_registration_durable(a.clone(), project(&a, &owner, &original))
        .await
        .unwrap();
    let mut replacement = original.clone();
    replacement.container_id = format!("bound-refresh-new-pod-{suffix}");
    let mut changed_a = (*writer.get(&a).unwrap()).clone();
    changed_a.set_container(Some(replacement.clone()));
    writer
        .insert_project_registration_durable(a.clone(), Arc::new(changed_a))
        .await
        .unwrap();
    let peer = store(&dsn).await;
    let mut changed_b = (*peer.get(&b).unwrap()).clone();
    assert!(changed_b.container_info().is_none());
    changed_b.set_container(Some(replacement.clone()));
    peer.insert_project_registration_durable(b.clone(), Arc::new(changed_b))
        .await
        .unwrap();
    let unrelated = writer.register_durable(vec![
        crate::pg::project_store::persist_ops::PersistOp::TouchProject {
            project_id: format!("unrelated-bound-{suffix}"),
            generation: "unrelated-generation".into(),
            last_activity: chrono::Utc::now(),
        },
    ]);
    let fetched = writer
        .refresh_project_registration(&b)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fetched.container_info().unwrap().container_id,
        replacement.container_id
    );
    assert_eq!(
        writer
            .get(&b)
            .unwrap()
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .physical_uid
            .as_deref(),
        Some(replacement.container_id.as_str())
    );
    assert_eq!(
        writer
            .insert_project_registration_durable(b.clone(), fetched)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert!(writer.get(&b).unwrap().sessions().contains(&sid));
    drop(unrelated);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn bound_successor_with_pending_work_can_continue_from_the_honest_local_predecessor() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("pending-successor-{suffix}");
    let original = basic(&id);
    let writer = store(&dsn).await;
    writer
        .insert_with_session_durable(
            id.clone(),
            project(&id, &id, &original),
            &format!("sid-{id}"),
        )
        .await
        .unwrap();
    let peer = store(&dsn).await;
    let captured = frozen(&writer.get(&id).unwrap());
    let mut proof = evidence(&original);
    proof.replacement.workload_uid = original.workload_uid.clone();
    assert_eq!(
        writer
            .replace_agent_container_registration_durable(&captured, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let unrelated = peer.register_durable(vec![
        crate::pg::project_store::persist_ops::PersistOp::TouchProject {
            project_id: format!("unrelated-successor-{suffix}"),
            generation: "unrelated-generation".into(),
            last_activity: chrono::Utc::now(),
        },
    ]);
    let refreshed = peer
        .refresh_project_registration(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        refreshed.container_info().unwrap().container_id,
        original.container_id,
        "unsafe-to-admit SQL is not returned as an already registered local view"
    );
    assert_eq!(
        refreshed
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .physical_uid
            .as_deref(),
        Some(original.container_id.as_str())
    );
    assert_eq!(
        peer.replace_agent_container_registration_durable(&refreshed, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(
        peer.get(&id)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        proof.replacement.container_id
    );
    drop(unrelated);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn project_registration_refresh_also_supports_projects_without_sessions() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!(
        "registration-without-session-{}",
        test_support::uuid_suffix()
    );
    let writer = store(&dsn).await;
    writer
        .insert_project_registration_durable(id.clone(), project(&id, &id, &basic(&id)))
        .await
        .unwrap();
    let reader = store(&dsn).await;
    let fetched = reader
        .refresh_project_registration(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.project_id(), id);
    assert!(fetched.sessions().is_empty());
    assert!(
        reader
            .refresh_project_registration("unknown-registration")
            .await
            .unwrap()
            .is_none()
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn poisoned_member_refresh_admits_a_proven_project_successor_with_pending_work() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("proven-refresh-owner-{suffix}");
    let a = format!("proven-refresh-a-{suffix}");
    let b = format!("proven-refresh-b-{suffix}");
    let sid = format!("proven-refresh-sid-{suffix}");
    let sid_a = format!("proven-refresh-sid-a-{suffix}");
    let original = basic(&owner);
    let writer = store(&dsn).await;
    writer
        .insert_with_session_durable(b.clone(), project(&b, &owner, &original), &sid)
        .await
        .unwrap();
    writer
        .insert_with_session_durable(a.clone(), project(&a, &owner, &original), &sid_a)
        .await
        .unwrap();
    let old_b = frozen(&writer.get(&b).unwrap());
    let mut proof = evidence(&original);
    proof.replacement.workload_uid = original.workload_uid.clone();
    let mut changed_a = (*writer.get(&a).unwrap()).clone();
    changed_a.set_container(Some(proof.replacement.clone()));
    writer
        .insert_project_registration_durable(a.clone(), Arc::new(changed_a))
        .await
        .unwrap();
    let peer = store(&dsn).await;
    let detached = frozen(&peer.get(&b).unwrap());
    assert!(detached.container_info().is_none());
    proof.previous = None;
    assert_eq!(
        peer.replace_agent_container_registration_durable(&detached, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    // A real SQL hydrate of another project refreshes the shared container
    // cursor but legitimately has no in-memory predecessor history. D's local
    // project projection remains old; its recovery cannot depend on that hint.
    let baseline = writer.inner.iter().into_iter().collect();
    let containers = writer.container_registration.lock().unwrap().clone();
    let key = sid_a.clone();
    let fetched_a = crate::pg::project_store::database::read(&writer.database, move |tx| {
        Box::pin(
            async move { crate::pg::project_store::repo::fetch_project_by_session(tx, &key).await },
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert!(
        writer
            .hydrate_fetched(&sid_a, fetched_a, &baseline, &containers)
            .unwrap()
            .is_some()
    );
    assert!(
        writer.container_registration.lock().unwrap()[&proof.replacement.container_name]
            .predecessor
            .is_none()
    );
    assert_eq!(
        writer.get(&b).unwrap().persistence_identity().generation,
        old_b.persistence_identity().generation
    );
    let unrelated = writer.register_durable(vec![
        crate::pg::project_store::persist_ops::PersistOp::TouchProject {
            project_id: format!("unrelated-proven-{suffix}"),
            generation: "unrelated-generation".into(),
            last_activity: chrono::Utc::now(),
        },
    ]);
    let refreshed = writer
        .refresh_project_registration(&b)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        refreshed.persistence_identity().generation,
        old_b.persistence_identity().generation
    );
    assert_eq!(
        writer.get(&b).unwrap().persistence_identity().generation,
        refreshed.persistence_identity().generation
    );
    assert_eq!(
        refreshed.container_info().unwrap().container_id,
        proof.replacement.container_id
    );
    assert!(refreshed.sessions().contains(&sid));
    drop(unrelated);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn registration_refresh_preserves_local_metadata_admitted_before_the_read() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("refresh-pending-metadata-{}", test_support::uuid_suffix());
    let writer = store(&dsn).await;
    writer
        .insert_project_registration_durable(id.clone(), project(&id, &id, &basic(&id)))
        .await
        .unwrap();
    let (release, barrier) = hold_advisory_lock(&dsn, format!("project:{id}")).await;
    let mut updated = (*writer.get(&id).unwrap()).clone();
    updated.set_request_id(Some("admitted-current-request".into()));
    writer.insert(id.clone(), Arc::new(updated)).unwrap();
    let refreshed = writer
        .refresh_project_registration(&id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refreshed.request_id(), Some("admitted-current-request"));
    assert_eq!(
        writer.get(&id).unwrap().request_id(),
        Some("admitted-current-request")
    );
    release.send(()).unwrap();
    barrier.await.unwrap();
    assert!(writer.wait_drained(Duration::from_secs(5)).await);
    let reader = store(&dsn).await;
    assert_eq!(
        reader.get(&id).unwrap().request_id(),
        Some("admitted-current-request")
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn delayed_registration_refresh_preserves_an_accepted_session_clear() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("refresh-clear-{}", test_support::uuid_suffix());
    let sid = format!("refresh-clear-sid-{id}");
    let writer = store(&dsn).await;
    writer
        .insert_with_session_durable(id.clone(), project(&id, &id, &basic(&id)), &sid)
        .await
        .unwrap();
    let baseline = writer.inner.iter().into_iter().collect();
    let containers = writer.container_registration.lock().unwrap().clone();
    let key = id.clone();
    let fetched = crate::pg::project_store::database::read(&writer.database, move |tx| {
        Box::pin(async move {
            crate::pg::project_store::repo::fetch_project_registration(tx, &key).await
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert!(writer.clear_session_one_durable(&id, &sid).await);
    let refreshed = writer
        .hydrate_registration_fetched(fetched, &baseline, &containers)
        .unwrap()
        .unwrap();
    assert!(
        !refreshed.sessions().contains(&sid),
        "delayed SQL cannot return a cleared session as current registration"
    );
    assert!(writer.get_by_session_id(&sid).is_none());
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn delayed_registration_refresh_cannot_resurrect_an_accepted_project_retirement() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let id = format!("refresh-retire-{}", test_support::uuid_suffix());
    let writer = store(&dsn).await;
    writer
        .insert_project_registration_durable(id.clone(), project(&id, &id, &basic(&id)))
        .await
        .unwrap();
    let baseline = writer.inner.iter().into_iter().collect();
    let containers = writer.container_registration.lock().unwrap().clone();
    let key = id.clone();
    let fetched = crate::pg::project_store::database::read(&writer.database, move |tx| {
        Box::pin(async move {
            crate::pg::project_store::repo::fetch_project_registration(tx, &key).await
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert!(writer.remove_durable(&id).await.is_some());
    assert!(
        writer
            .hydrate_registration_fetched(fetched, &baseline, &containers)
            .unwrap()
            .is_none()
    );
    assert!(writer.get(&id).is_none());
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn delayed_registration_refresh_cannot_roll_back_a_new_physical_generation() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let owner = format!("refresh-new-pod-{}", test_support::uuid_suffix());
    let a = format!("refresh-new-pod-a-{owner}");
    let b = format!("refresh-new-pod-b-{owner}");
    let original = basic(&owner);
    let writer = store(&dsn).await;
    for id in [&a, &b] {
        writer
            .insert_project_registration_durable(id.clone(), project(id, &owner, &original))
            .await
            .unwrap();
    }
    let baseline = writer.inner.iter().into_iter().collect();
    let containers = writer.container_registration.lock().unwrap().clone();
    let key = b.clone();
    let fetched = crate::pg::project_store::database::read(&writer.database, move |tx| {
        Box::pin(async move {
            crate::pg::project_store::repo::fetch_project_registration(tx, &key).await
        })
    })
    .await
    .unwrap()
    .unwrap();
    let captured_a = frozen(&writer.get(&a).unwrap());
    let mut proof = evidence(&original);
    proof.replacement.workload_uid = original.workload_uid.clone();
    assert_eq!(
        writer
            .replace_agent_container_registration_durable(&captured_a, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let canonical = writer.container_registration.lock().unwrap().clone();
    assert!(
        writer
            .hydrate_registration_fetched(fetched, &baseline, &containers)
            .unwrap()
            .is_none()
    );
    assert_eq!(*writer.container_registration.lock().unwrap(), canonical);
    assert_eq!(
        writer
            .get(&a)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        proof.replacement.container_id
    );
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}
