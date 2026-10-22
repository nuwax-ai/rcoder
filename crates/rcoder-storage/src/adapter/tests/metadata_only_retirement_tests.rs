use super::*;

fn register_physical(adapter: &ProjectAdapter, project_id: &str, name: &str, physical_uid: &str) {
    let mut info = create_test_info_with_container(project_id, name);
    let mut physical = info.container_info().unwrap();
    physical.container_id = physical_uid.into();
    info.set_container(Some(physical));
    adapter.insert(project_id.into(), Arc::new(info)).unwrap();
}

#[test]
fn explicit_container_metadata_retirement_preserves_unrelated_registration() {
    let adapter = ProjectAdapter::new(TEST_NAMESPACE.into(), TEST_CLUSTER_DOMAIN.into());
    register_physical(&adapter, "old-project-a", "shared-workload", "old-uid");
    register_physical(&adapter, "old-project-b", "shared-workload", "old-uid");
    register_physical(&adapter, "unrelated", "another-workload", "unrelated-uid");

    assert_eq!(adapter.delete_container_with_projects("old-uid"), (true, 2));
    assert!(adapter.get("old-project-a").is_none());
    assert!(adapter.get("old-project-b").is_none());
    assert!(!adapter.container_id_to_key.contains_key("old-uid"));
    assert!(!adapter.containers.contains_key("shared-workload"));
    assert_eq!(
        adapter
            .get("unrelated")
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "unrelated-uid"
    );
    assert_eq!(adapter.containers.len(), 1);
}

#[test]
fn repeated_old_metadata_retirement_does_not_remove_recreated_same_name() {
    let adapter = ProjectAdapter::new(TEST_NAMESPACE.into(), TEST_CLUSTER_DOMAIN.into());
    register_physical(&adapter, "project", "stable-workload", "old-uid");
    assert_eq!(adapter.delete_container_with_projects("old-uid"), (true, 1));

    // Recreate uses the same logical project/workload, but a new physical UID.
    // A repeated old metadata request must not retire that new registration.
    register_physical(&adapter, "project", "stable-workload", "new-uid");
    assert_eq!(
        adapter.delete_container_with_projects("old-uid"),
        (false, 0)
    );
    assert_eq!(
        adapter
            .get("project")
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "new-uid"
    );
    assert_eq!(
        adapter
            .container_id_to_key
            .get("new-uid")
            .unwrap()
            .value()
            .as_str(),
        "stable-workload"
    );
    assert_eq!(adapter.containers.len(), 1);
}
