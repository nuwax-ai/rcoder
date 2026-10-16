//! Real PG replacement must not wait for mirror sync to rescue a shared member.
use super::*;

fn pod_recreation(previous: &ContainerBasicInfo) -> VerifiedAgentReplacement {
    let mut proof = evidence(previous);
    proof.replacement.workload_uid = previous.workload_uid.clone();
    proof
}

#[tokio::test]
async fn same_workload_pod_recreation_migrates_both_members_without_sync() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("same-sts-owner-{suffix}");
    let a = format!("same-sts-a-{suffix}");
    let b = format!("same-sts-b-{suffix}");
    let sid_a = format!("same-sts-sid-a-{suffix}");
    let sid_b = format!("same-sts-sid-b-{suffix}");
    let original = basic(&owner);
    let writer = store(&dsn).await;
    for (id, sid, request) in [(&a, &sid_a, "metadata-a"), (&b, &sid_b, "metadata-b")] {
        let mut info = (*project(id, &owner, &original)).clone();
        info.set_request_id(Some(request.into()));
        writer
            .insert_with_session_durable(id.clone(), Arc::new(info), sid)
            .await
            .unwrap();
    }
    let captured_a = frozen(&writer.get(&a).unwrap());
    let captured_b = frozen(&writer.get(&b).unwrap());
    let proof = pod_recreation(&original);
    assert_eq!(
        proof.previous.as_ref().unwrap().workload_uid,
        proof.replacement.workload_uid
    );
    assert_eq!(
        writer
            .replace_agent_container_registration_durable(&captured_a, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(
        writer
            .get(&b)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        original.container_id,
        "the first member cannot overwrite another member's captured predecessor"
    );
    // No sync_once and no process reload between the two admitted requests.
    assert_eq!(
        writer
            .replace_agent_container_registration_durable(&captured_b, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let migrated_a = writer.get(&a).unwrap();
    let migrated_b = writer.get(&b).unwrap();
    let identity = migrated_a
        .persistence_identity()
        .container
        .as_ref()
        .unwrap();
    assert_eq!(
        identity.generation,
        migrated_b
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation
    );
    assert_ne!(
        identity.generation,
        captured_a
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation
    );
    assert_eq!(
        writer
            .replace_agent_container_registration_durable(&captured_a, &proof)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed,
        "repeating an already committed physical result is idempotent"
    );
    assert_eq!(
        writer
            .append_session_for_container_durable(&captured_b, "late-dead-pod-completion")
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    let outcomes = crate::pg::project_store::writer::execute_registered(
        &writer.database,
        vec![
            crate::pg::project_store::persist_ops::PersistOp::RemoveSession {
                session_id: sid_b.clone(),
                generation: captured_b.persistence_identity().sessions[&sid_b].clone(),
            },
            crate::pg::project_store::persist_ops::PersistOp::RemoveProject {
                project_id: b.clone(),
                generation: captured_b.persistence_identity().generation.clone(),
            },
        ],
    )
    .await
    .unwrap();
    assert_eq!(outcomes, 2);
    let reader = store(&dsn).await;
    for (id, sid, request) in [(&a, &sid_a, "metadata-a"), (&b, &sid_b, "metadata-b")] {
        let fresh = reader
            .get_by_session_id_with_fetch_result(sid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fresh.project_id(), id);
        assert_eq!(fresh.request_id(), Some(request));
        assert_eq!(
            fresh.container_info().unwrap().container_id,
            proof.replacement.container_id
        );
        assert_eq!(
            fresh.container_info().unwrap().workload_uid,
            original.workload_uid
        );
        assert_eq!(
            fresh
                .persistence_identity()
                .container
                .as_ref()
                .unwrap()
                .generation,
            identity.generation
        );
    }
    assert!(
        reader
            .get_by_session_id("late-dead-pod-completion")
            .is_none()
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(writer.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn same_workload_pod_recreation_concurrent_pg_replicas_share_one_generation() {
    let Some(dsn) = test_support::test_dsn().await else {
        return;
    };
    let suffix = test_support::uuid_suffix();
    let owner = format!("same-sts-race-owner-{suffix}");
    let a = format!("same-sts-race-a-{suffix}");
    let b = format!("same-sts-race-b-{suffix}");
    let original = basic(&owner);
    let first = store(&dsn).await;
    for id in [&a, &b] {
        first
            .insert_with_session_durable(
                id.clone(),
                project(id, &owner, &original),
                &format!("sid-{id}"),
            )
            .await
            .unwrap();
    }
    let peer = store(&dsn).await;
    let captured_a = frozen(&first.get(&a).unwrap());
    let captured_b = frozen(&peer.get(&b).unwrap());
    let proof = pod_recreation(&original);
    let (a_result, b_result) = tokio::join!(
        first.replace_agent_container_registration_durable(&captured_a, &proof),
        peer.replace_agent_container_registration_durable(&captured_b, &proof),
    );
    assert_eq!(a_result.unwrap(), PersistenceWriteOutcome::Committed);
    assert_eq!(b_result.unwrap(), PersistenceWriteOutcome::Committed);
    let reader = store(&dsn).await;
    let a_now = reader.get(&a).unwrap();
    let b_now = reader.get(&b).unwrap();
    assert_eq!(
        a_now
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation,
        b_now
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation
    );
    assert_eq!(
        a_now.container_info().unwrap().container_id,
        proof.replacement.container_id
    );
    assert_eq!(
        b_now.container_info().unwrap().container_id,
        proof.replacement.container_id
    );
    assert!(a_now.sessions().contains(&format!("sid-{a}")));
    assert!(b_now.sessions().contains(&format!("sid-{b}")));
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}
