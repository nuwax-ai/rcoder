use super::*;
use crate::{ProjectAdapter, ProjectStoreBackend};
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore};
use std::time::Duration;

fn memory_backend() -> ProjectStoreBackend {
    ProjectStoreBackend::Memory(Arc::new(ProjectAdapter::new(
        "test".into(),
        "cluster.local".into(),
    )))
}
fn key() -> WorkloadKey {
    WorkloadKey::new("test", ServiceType::ComputerAgentRunner, "runner-owner").unwrap()
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}
fn project(id: &str, uid: &str) -> Arc<ProjectAndContainerInfo> {
    let mut project = ProjectAndContainerInfo::new(id.into());
    project.set_user_id(Some("owner".into()));
    project.set_service_type(Some(ServiceType::ComputerAgentRunner));
    project.set_container(Some(ContainerBasicInfo {
        container_id: uid.into(),
        container_name: "runner-owner".into(),
        container_ip: String::new(),
        internal_port: 50051,
        external_port: 0,
        project_id: "owner".into(),
        status: "running".into(),
        service_url: "http://runner-owner".into(),
        created_at: chrono::Utc::now(),
        workload_uid: Some("controller".into()),
    }));
    Arc::new(project)
}

#[tokio::test]
async fn workload_shared_admission_blocks_exclusive_until_all_readers_exit() {
    let backend = memory_backend();
    let first = backend
        .acquire_workload_admission(key(), WorkloadAdmissionMode::Shared, deadline())
        .await
        .unwrap();
    let second = backend
        .acquire_workload_admission(key(), WorkloadAdmissionMode::Shared, deadline())
        .await
        .unwrap();
    let exclusive =
        backend.acquire_workload_admission(key(), WorkloadAdmissionMode::Exclusive, deadline());
    tokio::pin!(exclusive);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut exclusive)
            .await
            .is_err()
    );
    drop(first);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut exclusive)
            .await
            .is_err()
    );
    drop(second);
    let exclusive = exclusive.await.unwrap();
    assert!(!exclusive.has_canonical_references().await.unwrap());
}

#[tokio::test]
async fn workload_empty_frame_never_retires_a_new_registration() {
    let backend = memory_backend();
    let guard = backend
        .acquire_workload_admission(key(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .unwrap();
    let frame = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    assert!(frame.physical_uid().is_none());
    backend
        .insert("new".into(), project("new", "new-pod"))
        .unwrap();
    assert!(guard.has_canonical_references().await.unwrap());
    assert_eq!(
        backend
            .retire_workload_registration(&guard, &frame)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    assert_eq!(
        backend
            .get("new")
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "new-pod"
    );
}

#[tokio::test]
async fn workload_frame_preserves_same_uid_new_session_and_retires_exact_old_frame() {
    let backend = memory_backend();
    backend
        .insert("project".into(), project("project", "pod"))
        .unwrap();
    let guard = backend
        .acquire_workload_admission(key(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .unwrap();
    let old = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    backend.add_session_to_project("project", "fresh-opaque-session");
    assert_eq!(
        backend
            .retire_workload_registration(&guard, &old)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    let current = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    assert_eq!(current.session_ids(), vec!["fresh-opaque-session"]);
    assert_eq!(
        backend
            .retire_workload_registration(&guard, &current)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert!(backend.get("project").is_none());
    assert!(!guard.has_canonical_references().await.unwrap());
}

#[tokio::test]
async fn workload_guard_rejects_foreign_store_shared_retirement_and_expired_queue() {
    let backend = memory_backend();
    let other = memory_backend();
    let shared = backend
        .acquire_workload_admission(key(), WorkloadAdmissionMode::Shared, deadline())
        .await
        .unwrap();
    let frame = backend
        .capture_workload_registration(&shared, "runner-owner")
        .await
        .unwrap();
    assert!(
        other
            .capture_workload_registration(&shared, "runner-owner")
            .await
            .is_err()
    );
    assert!(
        backend
            .retire_workload_registration(&shared, &frame)
            .await
            .is_err()
    );
    assert!(
        backend
            .acquire_workload_admission(key(), WorkloadAdmissionMode::Exclusive, Instant::now())
            .await
            .is_err()
    );
    assert_eq!(
        WorkloadKey::new("test", ServiceType::ComputerNormalProject, "runner-owner").unwrap(),
        key()
    );
}
