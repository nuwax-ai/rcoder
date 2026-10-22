use super::*;
use shared_types::{ProjectAndContainerInfo, ProjectStore, ServiceType};
use std::sync::Arc;
fn deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(5)
}
fn fixture() -> ProjectStoreBackend {
    let inner = crate::ProjectAdapter::new("test".into(), "cluster.local".into());
    let mut app = ProjectAndContainerInfo::new("app-1".into());
    app.set_service_type(Some(ServiceType::UserappBuilder));
    app.set_request_id(Some("builder-metadata".into()));
    app.set_container(Some(shared_types::ContainerBasicInfo {
        container_id: "pod-1".into(),
        container_name: "builder-app-1".into(),
        container_ip: "127.0.0.1".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: "app-1".into(),
        status: "Running".into(),
        service_url: "http://127.0.0.1:50051".into(),
        created_at: Utc::now(),
        workload_uid: Some("sts-1".into()),
    }));
    inner.insert("app-1".into(), Arc::new(app)).unwrap();
    ProjectStoreBackend::Memory(Arc::new(inner))
}

#[tokio::test]
async fn userapp_pending_registration_preserves_binding_and_next_real_session_can_commit() {
    let store = fixture();
    let binding = store
        .accept_userapp_agent_binding("app-1", "life-1", "project-a", "user-1")
        .await
        .unwrap();
    assert!(
        store
            .register_userapp_agent_session(&binding, "pending", "invalid", deadline())
            .await
            .is_err()
    );
    assert!(
        store
            .get_userapp_agent_session("pending")
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.get("app-1").unwrap().sessions().is_empty());
    let current = store
        .get_userapp_agent_binding("app-1", "life-1", "project-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current, binding);
    let committed = store
        .register_userapp_agent_session(&binding, "pending-prefix-real", "real", deadline())
        .await
        .unwrap();
    assert_eq!(
        committed.current_session_id.as_deref(),
        Some("pending-prefix-real")
    );
}
#[tokio::test]
async fn parallel_conversations_share_only_the_builder_route() {
    let store = fixture();
    let a = store
        .accept_userapp_agent_binding("app-1", "life-1", "project-a", "user-1")
        .await
        .unwrap();
    let b = store
        .accept_userapp_agent_binding("app-1", "life-1", "project-b", "user-1")
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        store.register_userapp_agent_session(&a, "sid-a", "request-a", deadline()),
        store.register_userapp_agent_session(&b, "sid-b", "request-b", deadline())
    );
    assert_eq!(a.unwrap().current_session_id.as_deref(), Some("sid-a"));
    assert_eq!(b.unwrap().current_session_id.as_deref(), Some("sid-b"));
    assert_eq!(
        store
            .get_userapp_agent_session("sid-a")
            .await
            .unwrap()
            .unwrap()
            .0
            .project_id,
        "project-a"
    );
    assert_eq!(
        store
            .get_userapp_agent_session("sid-b")
            .await
            .unwrap()
            .unwrap()
            .0
            .project_id,
        "project-b"
    );
    let app = store.get("app-1").unwrap();
    assert_eq!(app.request_id(), Some("builder-metadata"));
    assert!(app.sessions().contains("sid-a") && app.sessions().contains("sid-b"));
    assert_eq!(store.get_stats().total_projects, 1);
    assert_eq!(store.get_stats().total_containers, 1);
}
#[tokio::test]
async fn historical_member_and_new_fallback_session_keep_the_same_owner() {
    let store = fixture();
    let original = store
        .accept_userapp_agent_binding("app-1", "life-1", "project-a", "user-1")
        .await
        .unwrap();
    let old = store
        .register_userapp_agent_session(&original, "sid-old", "old", deadline())
        .await
        .unwrap();
    let new = store
        .register_userapp_agent_session(&old, "sid-new", "new", deadline())
        .await
        .unwrap();
    assert_eq!(new.current_session_id.as_deref(), Some("sid-new"));
    assert_eq!(
        store
            .get_userapp_agent_session("sid-old")
            .await
            .unwrap()
            .unwrap()
            .0
            .binding_generation,
        new.binding_generation
    );
    assert_eq!(
        store
            .get_userapp_agent_session("sid-new")
            .await
            .unwrap()
            .unwrap()
            .0
            .project_id,
        "project-a"
    );
}
#[tokio::test]
async fn foreign_sid_claim_and_stale_stop_never_change_another_conversation() {
    let store = fixture();
    let a = store
        .accept_userapp_agent_binding("app-1", "life-1", "project-a", "user-1")
        .await
        .unwrap();
    let b = store
        .accept_userapp_agent_binding("app-1", "life-1", "project-b", "user-1")
        .await
        .unwrap();
    let a1 = store
        .register_userapp_agent_session(&a, "sid-a", "a1", deadline())
        .await
        .unwrap();
    assert!(
        store
            .register_userapp_agent_session(&b, "sid-a", "foreign", deadline())
            .await
            .is_err()
    );
    let a2 = store
        .register_userapp_agent_session(&a1, "sid-a2", "a2", deadline())
        .await
        .unwrap();
    assert!(
        !store
            .clear_userapp_agent_sessions(&a1, "old-stop", deadline())
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .get_userapp_agent_session("sid-a2")
            .await
            .unwrap()
            .unwrap()
            .0,
        a2
    );
    assert!(
        store
            .clear_userapp_agent_sessions(&a2, "new-stop", deadline())
            .await
            .unwrap()
    );
    assert!(
        store
            .get_userapp_agent_session("sid-a")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_userapp_agent_session("sid-a2")
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.get("app-1").unwrap().container_info().is_some());
}
#[tokio::test]
async fn receipt_replay_is_idempotent_and_route_replacement_is_not_success() {
    let store = fixture();
    let original = store
        .accept_userapp_agent_binding("app-1", "life-1", "project-a", "user-1")
        .await
        .unwrap();
    let first = store
        .register_userapp_agent_session(&original, "sid-a", "same-receipt", deadline())
        .await
        .unwrap();
    let replay = store
        .register_userapp_agent_session(&original, "sid-a", "same-receipt", deadline())
        .await
        .unwrap();
    assert_eq!(first, replay);
    assert!(
        store
            .register_userapp_agent_session(&original, "different-sid", "same-receipt", deadline())
            .await
            .is_err()
    );
    assert!(
        store
            .register_userapp_agent_session_for_route(
                &first,
                "sid-b",
                "route-race",
                deadline(),
                "replaced-app-generation"
            )
            .await
            .is_err()
    );
    assert!(
        store
            .get_userapp_agent_session("sid-b")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn same_captured_binding_and_actual_sid_confirm_without_rewriting_the_failed_receipt() {
    let store = fixture();
    let captured = store
        .accept_userapp_agent_binding("app-1", "life-1", "project-a", "user-1")
        .await
        .unwrap();
    let also_captured = store
        .accept_userapp_agent_binding("app-1", "life-1", "project-a", "user-1")
        .await
        .unwrap();
    assert_eq!(captured, also_captured);
    let first = store
        .register_userapp_agent_session(&captured, "same-sid", "first-prompt", deadline())
        .await
        .unwrap();
    // Both prompts were accepted before either mapping was registered. The
    // runtime reused one actual SID; the second old revision must still succeed.
    let second = store
        .register_userapp_agent_session(&also_captured, "same-sid", "second-prompt", deadline())
        .await
        .expect("the exact SID/route is already registered by the first prompt");
    assert_eq!(second, first);
    let failed_receipt = store
        .memory_mirror()
        .userapp_agents
        .lock()
        .unwrap()
        .receipts
        .get("second-prompt")
        .unwrap()
        .clone();
    assert!(
        !failed_receipt.1,
        "confirmation must not fabricate a committed CAS"
    );
    assert_eq!(
        store
            .register_userapp_agent_session(&captured, "same-sid", "second-prompt", deadline())
            .await
            .unwrap(),
        first
    );
    assert!(
        store
            .register_userapp_agent_session(&captured, "different-sid", "second-prompt", deadline())
            .await
            .is_err()
    );
    assert_eq!(store.get("app-1").unwrap().sessions().len(), 1);
    let newer = store
        .register_userapp_agent_session(&first, "newer-sid", "newer-prompt", deadline())
        .await
        .unwrap();
    assert!(
        store
            .register_userapp_agent_session(&captured, "same-sid", "second-prompt", deadline())
            .await
            .is_err()
    );
    assert_eq!(
        store
            .get_userapp_agent_binding("app-1", "life-1", "project-a")
            .await
            .unwrap()
            .unwrap(),
        newer
    );
    assert!(
        store
            .clear_userapp_agent_sessions(&newer, "stop", deadline())
            .await
            .unwrap()
    );
    assert!(
        store
            .register_userapp_agent_session(&captured, "same-sid", "second-prompt", deadline())
            .await
            .is_err()
    );
    assert!(store.get("app-1").unwrap().sessions().is_empty());
    assert_eq!(
        store
            .memory_mirror()
            .userapp_agents
            .lock()
            .unwrap()
            .receipts
            .get("second-prompt")
            .unwrap(),
        &failed_receipt
    );
}

#[tokio::test]
async fn same_sid_confirmation_cannot_accept_a_replaced_builder_route() {
    let store = fixture();
    let captured = store
        .accept_userapp_agent_binding("app-1", "life-1", "project-a", "user-1")
        .await
        .unwrap();
    let app_generation = store
        .get("app-1")
        .unwrap()
        .persistence_identity()
        .generation
        .clone();
    store
        .register_userapp_agent_session(&captured, "same-sid", "first", deadline())
        .await
        .unwrap();
    store
        .register_userapp_agent_session(&captured, "same-sid", "second", deadline())
        .await
        .unwrap();
    let mut replacement = ProjectAndContainerInfo::new("app-1".into());
    replacement.set_service_type(Some(ServiceType::UserappBuilder));
    replacement.set_request_id(Some("replacement-metadata".into()));
    store
        .memory_mirror()
        .insert("app-1".into(), Arc::new(replacement))
        .unwrap();
    assert!(
        store
            .register_userapp_agent_session_for_route(
                &captured,
                "same-sid",
                "second",
                deadline(),
                &app_generation
            )
            .await
            .is_err()
    );
    let app = store.get("app-1").unwrap();
    assert_eq!(app.request_id(), Some("replacement-metadata"));
    assert!(app.sessions().is_empty());
}
#[tokio::test]
async fn lifecycle_cleanup_is_exact_and_old_cleanup_receipt_cannot_target_new_bindings() {
    let store = fixture();
    let old = store
        .accept_userapp_agent_binding("app-1", "old-life", "old-project", "user-1")
        .await
        .unwrap();
    store
        .register_userapp_agent_session(&old, "old-sid", "old-reg", deadline())
        .await
        .unwrap();
    assert_eq!(
        store
            .invalidate_userapp_agent_bindings("app-1", "old-life", "cleanup", deadline())
            .await
            .unwrap(),
        1
    );
    let current = store
        .accept_userapp_agent_binding("app-1", "new-life", "new-project", "user-1")
        .await
        .unwrap();
    store
        .register_userapp_agent_session(&current, "new-sid", "new-reg", deadline())
        .await
        .unwrap();
    assert_eq!(
        store
            .invalidate_userapp_agent_bindings("app-1", "old-life", "cleanup", deadline())
            .await
            .unwrap(),
        0
    );
    assert!(
        store
            .get_userapp_agent_session("old-sid")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_userapp_agent_session("new-sid")
            .await
            .unwrap()
            .is_some()
    );
    assert!(store.get("app-1").is_some());
}
#[tokio::test]
async fn new_lifecycle_admission_retires_only_this_apps_stale_logical_ownership() {
    let store = fixture();
    let old = store
        .accept_userapp_agent_binding("app-1", "old-life", "same-project", "user-1")
        .await
        .unwrap();
    store
        .register_userapp_agent_session(&old, "old-sid", "old-reg", deadline())
        .await
        .unwrap();
    let new = store
        .accept_userapp_agent_binding("app-1", "new-life", "same-project", "user-1")
        .await
        .unwrap();
    assert_ne!(new.binding_generation, old.binding_generation);
    assert!(new.current_session_id.is_none());
    assert!(
        store
            .get_userapp_agent_session("old-sid")
            .await
            .unwrap()
            .is_none()
    );
    let current = store
        .register_userapp_agent_session(&new, "new-sid", "new-reg", deadline())
        .await
        .unwrap();
    assert_eq!(
        store
            .invalidate_userapp_agent_bindings("app-1", "old-life", "late-cleanup", deadline())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .get_userapp_agent_session("new-sid")
            .await
            .unwrap()
            .unwrap()
            .0,
        current
    );
    assert!(
        store
            .accept_userapp_agent_binding("foreign-app", "other-life", "same-project", "user-1")
            .await
            .is_err()
    );
}
#[tokio::test]
async fn memory_append_after_route_replacement_never_overwrites_replacement_metadata() {
    let store = fixture();
    let old = store.get("app-1").unwrap();
    let old_generation = old.persistence_identity().generation.clone();
    let mut replacement = ProjectAndContainerInfo::new("app-1".into());
    replacement.set_service_type(Some(ServiceType::UserappBuilder));
    replacement.set_request_id(Some("replacement-metadata".into()));
    replacement.set_container(old.container_info());
    store.insert("app-1".into(), Arc::new(replacement)).unwrap();
    // Reproduce replacement between a captured app read and the atomic append.
    assert!(
        !store
            .memory_mirror()
            .add_userapp_session_if_generation(
                "app-1",
                &old_generation,
                "stale-sid",
                "stale-generation"
            )
            .unwrap()
    );
    let current = store.get("app-1").unwrap();
    assert_eq!(current.request_id(), Some("replacement-metadata"));
    assert!(!current.sessions().contains("stale-sid"));
    let new_generation = current.persistence_identity().generation.clone();
    assert!(
        store
            .memory_mirror()
            .add_userapp_session_if_generation(
                "app-1",
                &new_generation,
                "new-sid",
                "new-generation"
            )
            .unwrap()
    );
    assert_eq!(
        store.get("app-1").unwrap().request_id(),
        Some("replacement-metadata")
    );
}
#[tokio::test]
async fn project_user_and_app_conflicts_are_explicit_without_alias_projects() {
    let store = fixture();
    store
        .accept_userapp_agent_binding("app-1", "life-1", "project-a", "user-1")
        .await
        .unwrap();
    assert!(
        store
            .accept_userapp_agent_binding("app-1", "life-1", "project-a", "user-2")
            .await
            .is_err()
    );
    assert!(
        store
            .accept_userapp_agent_binding("app-2", "life-2", "project-a", "user-1")
            .await
            .is_err()
    );
    assert!(
        store
            .get_userapp_agent_session("unclaimed-legacy-sid")
            .await
            .unwrap()
            .is_none()
    );
}
