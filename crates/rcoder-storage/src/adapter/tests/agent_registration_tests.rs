use super::*;

fn owned_project(id: &str, owner: &str) -> ProjectAndContainerInfo {
    let mut info = create_test_info_with_container(id, &format!("computer-{owner}"));
    info.set_service_type(Some(ServiceType::ComputerAgentRunner));
    info.set_user_id(Some(owner.to_owned()));
    let mut basic = info.container_info().unwrap();
    basic.project_id = owner.to_owned();
    basic.workload_uid = Some(format!("old-workload-{owner}"));
    info.set_container(Some(basic));
    info
}

#[test]
fn memory_completion_preserves_current_metadata_and_rejects_retirement() {
    let adapter = make_adapter();
    let initial = owned_project("chat-project", "shared-user");
    adapter
        .insert("chat-project".into(), Arc::new(initial.clone()))
        .unwrap();
    let mut expected = (*adapter.get("chat-project").unwrap()).clone();
    expected.set_container(adapter.get("chat-project").unwrap().container_info());
    let mut newer = expected.clone();
    newer.set_request_id(Some("latest-request".into()));
    newer.add_session("concurrent-session");
    adapter
        .insert("chat-project".into(), Arc::new(newer))
        .unwrap();
    assert!(
        adapter
            .append_session_for_container(&expected, "completed-session")
            .unwrap()
    );
    let current = adapter.get("chat-project").unwrap();
    assert_eq!(current.request_id(), Some("latest-request"));
    assert!(current.sessions().contains("concurrent-session"));
    assert!(current.sessions().contains("completed-session"));
    let mut before_clear = (*current).clone();
    before_clear.set_container(current.container_info());
    adapter.clear_session_one("chat-project", "completed-session");
    assert!(
        !adapter
            .append_session_for_container(&before_clear, "completed-session")
            .unwrap()
    );
    adapter.remove("chat-project");
    assert!(
        !adapter
            .append_session_for_container(&expected, "late-session")
            .unwrap()
    );
}

#[test]
fn memory_shared_replacement_retains_predecessor_snapshots_and_new_entry() {
    let adapter = make_adapter();
    adapter
        .insert("first".into(), Arc::new(owned_project("first", "shared")))
        .unwrap();
    adapter
        .insert("second".into(), Arc::new(owned_project("second", "shared")))
        .unwrap();
    adapter.add_session_to_project("first", "first-session");
    adapter.add_session_to_project("second", "second-session");
    let freeze = |id: &str| {
        let info = adapter.get(id).unwrap();
        let mut frozen = (*info).clone();
        frozen.set_container(info.container_info());
        frozen
    };
    let first = freeze("first");
    let second = freeze("second");
    let old = first.container_info().unwrap();
    let mut replacement = old.clone();
    replacement.container_id = "new-physical-pod".into();
    replacement.workload_uid = Some("new-physical-workload".into());
    let proof = shared_types::VerifiedAgentReplacement {
        identifier: "shared".into(),
        service_type: ServiceType::ComputerAgentRunner,
        previous: Some(old.clone()),
        replacement: replacement.clone(),
        workspace: shared_types::AgentWorkspaceIdentity {
            namespace: TEST_NAMESPACE.into(),
            claim_name: "owned-workspace".into(),
            claim_uid: "owned-claim".into(),
            mount_path: "/home/user".into(),
            sub_path: None,
        },
    };
    assert!(
        adapter
            .replace_agent_container_registration(&first, &proof)
            .unwrap()
    );
    assert_eq!(
        adapter
            .get("second")
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        old.container_id
    );
    assert!(
        !adapter
            .append_session_for_container(&first, "late-old-response")
            .unwrap()
    );
    assert!(
        adapter
            .replace_agent_container_registration(&second, &proof)
            .unwrap()
    );
    let first_current = adapter.get("first").unwrap();
    let second_current = adapter.get("second").unwrap();
    assert_eq!(
        first_current
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation,
        second_current
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation
    );
    assert_eq!(
        adapter
            .get_by_session_id("first-session")
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        replacement.container_id
    );
    assert_eq!(
        adapter
            .get_by_session_id("second-session")
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        replacement.container_id
    );
    adapter.remove("second");
    assert_eq!(
        adapter
            .containers
            .get(&replacement.container_name)
            .unwrap()
            .info()
            .container_id,
        replacement.container_id
    );
    assert_eq!(
        adapter
            .containers
            .get(&replacement.container_name)
            .unwrap()
            .ref_count(),
        1
    );
    assert_eq!(
        adapter
            .get_by_session_id("first-session")
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        replacement.container_id
    );
}
