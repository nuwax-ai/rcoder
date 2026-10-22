//! The real PG Stop committed, but its caller has not received completion yet.
use super::complete_project_stop_registration;
use crate::grpc::SessionStreamRegistry;
use rcoder_storage::{ProjectStoreBackend, config::PostgresConfig, pg::PgStore};
use shared_types::{
    ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore as _, ServiceType,
    persistence::PersistenceWriteOutcome,
};
use std::{sync::Arc, time::Duration};

async fn late_stop_completion(initial_claim: bool) {
    let dsn = std::env::var("RCODER_PG_TEST_DSN")
        .ok()
        .filter(|dsn| !dsn.is_empty());
    assert!(
        dsn.is_some() || std::env::var("RCODER_PG_TEST_STRICT").as_deref() != Ok("1"),
        "Strict PG tests require an isolated DSN"
    );
    let Some(dsn) = dsn else { return };
    let config = PostgresConfig {
        url: Some(dsn),
        ..Default::default()
    };
    let store = Arc::new(
        PgStore::connect(&config, "stop-claim-test".into(), "cluster.local".into())
            .await
            .unwrap(),
    );
    let backend = Arc::new(ProjectStoreBackend::Postgres(store.clone()));
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let project_id = format!("stop-claim-project-{suffix}");
    let owner = format!("stop-claim-owner-{suffix}");
    let sid = format!("stop-claim-sid-{suffix}");
    let physical = ContainerBasicInfo {
        container_id: format!("stop-claim-pod-{suffix}"),
        container_name: format!("stop-claim-workload-{suffix}"),
        container_ip: "127.0.0.1".into(),
        internal_port: shared_types::GRPC_DEFAULT_PORT,
        external_port: 0,
        project_id: owner.clone(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: "http://127.0.0.1:50051".into(),
        workload_uid: Some(format!("stop-claim-sts-{suffix}")),
    };
    let mut project = ProjectAndContainerInfo::new(project_id.clone());
    project.set_user_id(Some(owner));
    project.set_service_type(Some(ServiceType::ComputerAgentRunner));
    project.set_container(Some(physical.clone()));
    assert_eq!(
        backend
            .insert_project_registration_durable(project_id.clone(), Arc::new(project))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let captured = backend.get(&project_id).unwrap();
    assert_eq!(
        backend
            .append_session_for_container_durable(&captured, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let frame = backend
        .capture_project_stop_registration(&project_id)
        .await
        .unwrap()
        .unwrap();
    let old_generation = frame.project().persistence_identity().generation.clone();
    let old_container = frame.project().persistence_identity().container.clone();
    let streams = Arc::new(SessionStreamRegistry::new());
    if initial_claim {
        assert!(streams.claim_first_client(&sid));
    }
    let committed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let clearing = tokio::spawn({
        let backend = backend.clone();
        let streams = streams.clone();
        let committed = committed.clone();
        let release = release.clone();
        async move {
            complete_project_stop_registration(&streams, &frame, async {
                let outcome = backend.clear_sessions_if_current_durable(&frame).await?;
                assert_eq!(outcome, PersistenceWriteOutcome::Committed);
                committed.notify_one();
                release.notified().await;
                Ok(outcome)
            })
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(10), committed.notified())
        .await
        .unwrap();
    let successor = backend
        .capture_project_stop_registration(&project_id)
        .await
        .unwrap()
        .unwrap();
    let current = successor.project();
    assert_ne!(current.persistence_identity().generation, old_generation);
    assert_eq!(current.persistence_identity().container, old_container);
    assert_eq!(
        current.container_info().unwrap().container_id,
        physical.container_id
    );
    assert_eq!(
        current.container_info().unwrap().workload_uid,
        physical.workload_uid
    );
    assert!(current.sessions().is_empty());
    let mut fresh = current.clone();
    fresh.set_request_id(Some("fresh-request-after-committed-stop".into()));
    assert_eq!(
        backend
            .insert_project_registration_durable(project_id.clone(), Arc::new(fresh))
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let accepted = backend.get(&project_id).unwrap();
    assert_eq!(
        backend
            .append_session_for_container_durable(&accepted, &sid)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    // A fresh same-SID subscription happens after the real SQL epoch transition.
    assert_eq!(streams.claim_first_client(&sid), !initial_claim);
    release.notify_one();
    assert_eq!(
        clearing.await.unwrap().unwrap(),
        PersistenceWriteOutcome::Committed
    );
    let fresh_claim_survived = !streams.claim_first_client(&sid);
    let reader = Arc::new(
        PgStore::connect(&config, "stop-claim-test".into(), "cluster.local".into())
            .await
            .unwrap(),
    );
    let routed = reader
        .get_by_session_id_with_fetch_result(&sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        routed.request_id(),
        Some("fresh-request-after-committed-stop")
    );
    assert_ne!(routed.persistence_identity().generation, old_generation);
    assert_eq!(routed.latest_session(), Some(sid.as_str()));
    assert_eq!(
        routed.container_info().unwrap().container_id,
        physical.container_id
    );
    assert!(
        reader
            .shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
    assert!(
        store
            .shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
    assert!(
        fresh_claim_survived,
        "late Committed Stop released a later same-SID claim"
    );
}

#[tokio::test]
async fn committed_stop_preserves_new_same_sid_claim_after_sql_commit() {
    late_stop_completion(true).await;
}

#[tokio::test]
async fn committed_stop_does_not_release_a_claim_absent_at_capture() {
    late_stop_completion(false).await;
}
