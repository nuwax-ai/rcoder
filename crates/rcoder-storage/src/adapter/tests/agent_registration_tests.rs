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
fn legacy_pending_history_is_retained_but_never_published_as_session_routing() {
    let adapter = make_adapter();
    let mut old = owned_project("legacy-state", "legacy-user");
    old.restore_session("pending");
    let original = old.persistence_identity().clone();
    adapter
        .insert("legacy-state".into(), Arc::new(old))
        .unwrap();
    let captured = adapter.get("legacy-state").unwrap();
    let retained = captured.persistence_identity();
    assert_eq!(retained.generation, original.generation);
    assert_eq!(retained.revision, original.revision);
    assert_eq!(retained.predecessor, original.predecessor);
    assert_eq!(retained.container, original.container);
    assert_eq!(retained.sessions, original.sessions);
    assert_eq!(retained.retired_sessions, original.retired_sessions);
    assert_eq!(captured.session_id(), Some("pending"));
    assert!(adapter.get_by_session_id("pending").is_none());
    assert!(adapter.get_container_name_by_session("pending").is_none());
    assert!(adapter.restore_session_to_project("legacy-state", "pending"));
    assert!(adapter.get_by_session_id("pending").is_none());
    assert!(adapter.session_index.get("pending").is_none());
    assert!(
        adapter
            .append_session_for_container(&captured, "fresh-real-session")
            .unwrap()
    );
    let current = adapter.get("legacy-state").unwrap();
    assert!(
        current.sessions().contains("pending"),
        "retain original history"
    );
    assert_eq!(current.latest_session(), Some("fresh-real-session"));
    assert!(adapter.get_by_session_id("fresh-real-session").is_some());
}

#[test]
fn new_pending_registration_does_not_publish_state_or_weaken_foreign_session_guards() {
    let adapter = make_adapter();
    let old = owned_project("new-chat", "chat-user");
    adapter.insert("new-chat".into(), Arc::new(old)).unwrap();
    let captured = adapter.get("new-chat").unwrap();
    assert!(
        adapter
            .append_session_for_container(&captured, "pending")
            .is_err()
    );
    assert!(!adapter.add_session_to_project("new-chat", "pending"));
    assert!(adapter.get("new-chat").unwrap().sessions().is_empty());
    assert!(adapter.get_by_session_id("pending").is_none());
    assert!(adapter.add_session_to_project("new-chat", "opaque/real-session"));
    adapter
        .insert(
            "other-chat".into(),
            Arc::new(owned_project("other-chat", "other-user")),
        )
        .unwrap();
    let other = adapter.get("other-chat").unwrap();
    assert!(
        adapter
            .append_session_for_container(&other, "opaque/real-session")
            .is_err()
    );
    assert!(
        adapter
            .append_session_for_container(&other, "pending-prefix-is-opaque")
            .unwrap()
    );
    let rejected = owned_project("rejected-pending", "new-owner");
    assert!(
        adapter
            .insert_with_session(
                "rejected-pending".into(),
                Arc::new(rejected),
                Some("pending")
            )
            .is_err()
    );
    assert!(
        adapter.get("rejected-pending").is_none(),
        "reject before publishing project/container state"
    );
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

fn assert_memory_shared_replacement_retains_predecessor_snapshots_and_new_entry(
    same_workload: bool,
) {
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
    if !same_workload {
        replacement.workload_uid = Some("new-physical-workload".into());
    }
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
    let new_first = freeze("first");
    assert!(
        adapter
            .append_session_for_container(&new_first, "first-session")
            .unwrap(),
        "a new message on the migrated original SID must not be blocked by its retired old generation"
    );
    assert!(
        !adapter
            .append_session_for_container(&first, "first-session")
            .unwrap(),
        "an old completion on that same SID must remain fenced"
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

#[test]
fn memory_shared_replacement_retains_predecessor_snapshots_and_new_entry() {
    assert_memory_shared_replacement_retains_predecessor_snapshots_and_new_entry(false);
}

#[test]
fn memory_same_statefulset_pod_recreation_rebinds_shared_projects_and_fences_old_completion() {
    assert_memory_shared_replacement_retains_predecessor_snapshots_and_new_entry(true);
}
