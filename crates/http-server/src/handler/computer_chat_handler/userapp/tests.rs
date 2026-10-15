#![cfg(feature = "userapp-turso")]
use super::prepare_conversation;
use crate::handler::userapp_agent_target::test_support::{APP_ID, Fixture, USER_ID};
use shared_types::{ComputerChatRequest, UserAppLifecycleStore as _};

fn request(project: Option<&str>, sid: Option<&str>) -> ComputerChatRequest {
    serde_json::from_value(serde_json::json!({
        "user_id": USER_ID, "app_id": APP_ID, "service_type": "userapp",
        "project_id": project, "session_id": sid, "prompt": "protocol test"
    }))
    .unwrap()
}

#[tokio::test]
async fn foreign_and_legacy_session_hints_create_this_project_without_mutating_foreign_binding() {
    let fixture = Fixture::new().await;
    let original = fixture.bind("cid-a", "sid-a").await;
    let application = fixture
        .store
        .get_application(APP_ID)
        .await
        .unwrap()
        .unwrap();
    for hint in ["sid-a", "legacy-shared-sid"] {
        let mut input = request(Some("cid-b"), Some(hint));
        let (binding, project, fresh) = prepare_conversation(
            &fixture.state,
            APP_ID,
            &application.lifecycle_id,
            &mut input,
        )
        .await
        .unwrap();
        assert_eq!(project, "cid-b");
        assert_eq!(binding.project_id, "cid-b");
        assert!(fresh);
        assert_eq!(input.session_id, None);
        let foreign = fixture
            .state
            .projects
            .get_userapp_agent_session("sid-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(foreign.0, original);
        assert!(
            fixture.agent.calls.lock().unwrap().is_empty(),
            "admission cannot cancel or load the foreign agent"
        );
    }
}

#[tokio::test]
async fn historical_same_owner_sid_can_resume_even_when_the_current_sid_is_newer() {
    let fixture = Fixture::new().await;
    let first = fixture.bind("cid-a", "sid-old").await;
    let current = fixture
        .state
        .projects
        .register_userapp_agent_session(
            &first,
            "sid-new",
            "register-new",
            tokio::time::Instant::now() + std::time::Duration::from_secs(10),
        )
        .await
        .unwrap();
    let mut input = request(Some("cid-a"), Some("sid-old"));
    let (binding, project, fresh) =
        prepare_conversation(&fixture.state, APP_ID, &current.lifecycle_id, &mut input)
            .await
            .unwrap();
    assert_eq!(project, "cid-a");
    assert_eq!(binding.current_session_id.as_deref(), Some("sid-new"));
    assert_eq!(input.session_id.as_deref(), Some("sid-old"));
    assert!(
        !fresh,
        "runtime absence or a newer current pointer must not prevent historical load"
    );
    let mut omitted = request(Some("cid-a"), None);
    let (_, _, fresh) =
        prepare_conversation(&fixture.state, APP_ID, &current.lifecycle_id, &mut omitted)
            .await
            .unwrap();
    assert!(!fresh);
    assert_eq!(omitted.session_id.as_deref(), Some("sid-new"));
}

#[tokio::test]
async fn missing_project_is_inferred_only_from_its_own_session_otherwise_generated_once() {
    let fixture = Fixture::new().await;
    let binding = fixture.bind("cid-a", "sid-a").await;
    let mut input = request(None, Some("sid-a"));
    let (_, project, fresh) =
        prepare_conversation(&fixture.state, APP_ID, &binding.lifecycle_id, &mut input)
            .await
            .unwrap();
    assert_eq!(project, "cid-a");
    assert!(!fresh);
    let mut legacy = request(None, Some("unknown"));
    let (_, project, fresh) =
        prepare_conversation(&fixture.state, APP_ID, &binding.lifecycle_id, &mut legacy)
            .await
            .unwrap();
    assert_ne!(project, APP_ID);
    assert!(uuid::Uuid::parse_str(&project).is_ok());
    assert!(fresh);
    assert_eq!(legacy.project_id.as_deref(), Some(project.as_str()));
    let request_id = legacy.request_id.clone();
    let (_, repeated, _) =
        prepare_conversation(&fixture.state, APP_ID, &binding.lifecycle_id, &mut legacy)
            .await
            .unwrap();
    assert_eq!(repeated, project);
    assert_eq!(legacy.request_id, request_id);
}
