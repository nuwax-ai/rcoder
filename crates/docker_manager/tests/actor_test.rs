use chrono::Utc;
use docker_manager::{ContainerStateActor, ContainerStatus, DockerContainerInfo};
use std::collections::HashMap;

fn create_test_info(project_id: &str) -> DockerContainerInfo {
    DockerContainerInfo {
        container_id: format!("test_container_id_{}", project_id),
        container_name: format!("test_container_name_{}", project_id),
        project_id: project_id.to_string(),
        user_id: None,
        service_type: None,
        image: "test_image".to_string(),
        status: ContainerStatus::Running,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        host_path: "/tmp/host".to_string(),
        container_path: "/app".to_string(),
        port_bindings: HashMap::new(),
        assigned_port: 8080,
        health_status: Some("healthy".to_string()),
        service_health: None,
        internal_port: 8080,
        network_name: "test_net".to_string(),
    }
}

#[tokio::test]
async fn test_actor_workflow() {
    // 1. Create Actor
    let (actor, handle) = ContainerStateActor::new();

    // 2. Spawn Actor
    let actor_handle = tokio::spawn(actor.run());

    // 3. Test: Initially empty
    assert_eq!(handle.len().await, 0);
    assert!(handle.is_empty().await);

    // 4. Test: Insert
    let info1 = create_test_info("p1");
    handle.insert("p1".to_string(), info1.clone()).await;

    assert_eq!(handle.len().await, 1);
    assert!(!handle.is_empty().await);
    assert!(handle.contains_key("p1").await);

    // 5. Test: Get
    let retrieved = handle.get("p1").await;
    assert!(retrieved.is_some());
    let r = retrieved.unwrap();
    assert_eq!(r.container_id, info1.container_id);
    assert_eq!(r.project_id, "p1");

    // 6. Test: List & Keys
    let info2 = create_test_info("p2");
    handle.insert("p2".to_string(), info2).await;

    let list = handle.list().await;
    assert_eq!(list.len(), 2);

    let keys = handle.keys().await;
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&"p1".to_string()));
    assert!(keys.contains(&"p2".to_string()));

    // 7. Test: Remove
    let removed = handle.remove("p1").await;
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().project_id, "p1");

    assert_eq!(handle.len().await, 1);
    assert!(!handle.contains_key("p1").await);
    assert!(handle.contains_key("p2").await);

    // 8. Test: Remove non-existent
    let removed_none = handle.remove("non_existent").await;
    assert!(removed_none.is_none());

    // Clean up (Actor will stop when handle is dropped, but we can just let test finish)
    drop(handle);
    let _ = actor_handle.await;
}

#[tokio::test]
async fn test_actor_update() {
    let (actor, handle) = ContainerStateActor::new();
    tokio::spawn(actor.run());

    let mut info = create_test_info("p1");
    handle.insert("p1".to_string(), info.clone()).await;

    // Modify info
    info.status = ContainerStatus::Stopped;

    // Update if exists
    let updated = handle.update_if_exists("p1", info.clone()).await;
    assert!(updated);

    let retrieved = handle.get("p1").await.unwrap();
    assert_eq!(retrieved.status, ContainerStatus::Stopped);

    // Update non-existent
    let updated_fail = handle.update_if_exists("p2", info).await;
    assert!(!updated_fail);
}

#[tokio::test]
async fn test_remove_if_container_id() {
    let (actor, handle) = ContainerStateActor::new();
    tokio::spawn(actor.run());

    let info = create_test_info("p1");
    // info.container_id is "test_container_id_p1"
    handle.insert("p1".to_string(), info.clone()).await;

    // 1. Try remove with WRONG container_id
    let removed = handle.remove_if_container_id("p1", "wrong_id").await;
    assert!(removed.is_none());

    // Verify still exists
    assert!(handle.get("p1").await.is_some());

    // 2. Try remove with CORRECT container_id
    let removed = handle
        .remove_if_container_id("p1", "test_container_id_p1")
        .await;
    assert!(removed.is_some());

    // Verify removed
    assert!(handle.get("p1").await.is_none());
}
