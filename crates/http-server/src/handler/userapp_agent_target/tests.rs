use super::{resolve_agent_target, resolve_userapp_session_target, test_support::*};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::Ordering};
use tower::ServiceExt as _;

fn controls(state: Arc<crate::app_state::AppState>) -> Router {
    Router::new()
        .route("/status", post(crate::handler::computer_agent_status))
        .route(
            "/cancel",
            post(crate::handler::computer_agent_session_cancel),
        )
        .route("/stop", post(crate::handler::computer_agent_stop))
        .route(
            "/permission",
            post(crate::handler::computer_notify_resolved),
        )
        .with_state(state)
}

async fn post_body(router: &Router, path: &str, body: Value) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, body)
}

fn permission(project: &str, sid: &str) -> Value {
    json!({"user_id": USER_ID, "project_id": project,
        "permission_resolve_request": {"session_id": sid, "tool_call_id": "call-1",
            "request_permission_response": {"outcome": {"outcome": "cancelled"}}}})
}

#[tokio::test]
async fn java_controls_without_app_fields_route_each_conversation_to_its_builder_agent() {
    let fixture = Fixture::new().await;
    fixture.bind("cid-a", "sid-a").await;
    fixture.bind("cid-b", "sid-b").await;
    let router = controls(fixture.state.clone());
    let (status, body) = post_body(
        &router,
        "/status",
        json!({"user_id": USER_ID, "project_id": "cid-a"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["project_id"], "cid-a");
    assert_eq!(body["data"]["session_id"], "sid-a");
    assert_eq!(body["data"]["is_alive"], true);

    // The Java cancellation request uses query parameters with an empty POST body.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/cancel?user_id={USER_ID}&project_id=cid-a&session_id=sid-a"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["data"]["success"], true);
    let (status, body) = post_body(&router, "/permission", permission("cid-a", "sid-a")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["success"], true);
    let (status, body) = post_body(
        &router,
        "/stop",
        json!({"user_id": USER_ID, "project_id": "cid-a", "session_id": "sid-a"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["success"], true);
    assert_eq!(body["data"]["project_id"], "cid-a");

    let calls = fixture.agent.calls.lock().unwrap().clone();
    assert!(
        calls
            .iter()
            .any(|call| matches!(call, AgentCall::Status(req) if req.project_id == "cid-a"))
    );
    assert!(calls.iter().any(|call| matches!(call, AgentCall::Cancel(req) if req.project_id == "cid-a" && req.session_id == "sid-a")));
    assert!(calls.iter().any(|call| matches!(call, AgentCall::Permission(req) if req.project_id == "cid-a" && req.session_id == "sid-a")));
    assert!(calls.iter().any(|call| matches!(call, AgentCall::Stop(req) if req.project_id == "cid-a" && req.expected_session_id.as_deref() == Some("sid-a"))));
    drop(calls);
    assert_eq!(
        fixture
            .agent
            .sessions
            .lock()
            .unwrap()
            .get("cid-b")
            .map(String::as_str),
        Some("sid-b")
    );
    assert!(!fixture.agent.sessions.lock().unwrap().contains_key("cid-a"));
    let binding_b = fixture
        .state
        .projects
        .find_userapp_agent_binding(Some(USER_ID), "cid-b")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding_b.current_session_id.as_deref(), Some("sid-b"));
    let binding_a = fixture
        .state
        .projects
        .find_userapp_agent_binding(Some(USER_ID), "cid-a")
        .await
        .unwrap()
        .unwrap();
    assert!(binding_a.current_session_id.is_none());
    assert!(fixture.runtime.lookups.lock().unwrap().iter().all(
        |(app, service)| app == APP_ID && *service == shared_types::ServiceType::UserappBuilder
    ));
    assert_eq!(fixture.runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn foreign_or_unknown_sid_never_cancels_stops_or_resolves_another_conversation() {
    let fixture = Fixture::new().await;
    fixture.bind("cid-a", "sid-a").await;
    fixture.bind("cid-b", "sid-b").await;
    let router = controls(fixture.state.clone());
    for sid in ["sid-b", "unknown-sid"] {
        let (status, body) = post_body(
            &router,
            "/status",
            json!({"user_id": USER_ID, "project_id": "cid-a", "session_id": sid}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["is_alive"], false);
        assert_eq!(body["data"]["project_id"], "cid-a");
        let (status, body) = post_body(
            &router,
            "/cancel",
            json!({"user_id": USER_ID, "project_id": "cid-a", "session_id": sid}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["success"], true);
        let (_, body) = post_body(
            &router,
            "/stop",
            json!({"user_id": USER_ID, "project_id": "cid-a", "session_id": sid}),
        )
        .await;
        assert_ne!(body["code"], "0000");
        let (status, _) = post_body(&router, "/permission", permission("cid-a", sid)).await;
        assert!(!status.is_success());
    }
    assert!(fixture.agent.calls.lock().unwrap().is_empty());
    assert!(fixture.runtime.lookups.lock().unwrap().is_empty());
    assert_eq!(fixture.agent.sessions.lock().unwrap().len(), 2);
    assert_eq!(fixture.runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn explicit_app_without_binding_does_not_fall_back_to_computer_or_create_a_builder() {
    let fixture = Fixture::new().await;
    let router = controls(fixture.state.clone());
    let (status, body) = post_body(
        &router,
        "/status",
        json!({"user_id": USER_ID, "service_type": "userapp", "app_id": APP_ID}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["is_alive"], false);
    assert_eq!(
        body["data"]["project_id"], "",
        "an application ID cannot stand in for an unknown conversation ID"
    );
    let (status, body) = post_body(
        &router,
        "/cancel",
        json!({"user_id": USER_ID, "service_type": "userapp", "app_id": APP_ID}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["success"], true);
    let (_, body) = post_body(
        &router,
        "/stop",
        json!({"user_id": USER_ID, "service_type": "userapp", "app_id": APP_ID}),
    )
    .await;
    assert_ne!(body["code"], "0000");
    let mut request = permission("unknown-cid", "unknown-sid");
    request["service_type"] = "userapp".into();
    request["app_id"] = APP_ID.into();
    let (status, _) = post_body(&router, "/permission", request).await;
    assert!(!status.is_success());
    assert!(fixture.agent.calls.lock().unwrap().is_empty());
    assert!(fixture.runtime.lookups.lock().unwrap().is_empty());
    assert_eq!(fixture.runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn session_only_resolution_finds_the_shared_builder_and_preserves_conversation_identity() {
    let fixture = Fixture::new().await;
    fixture.bind("cid-a", "sid-a").await;
    fixture.bind("cid-b", "sid-b").await;
    let target = resolve_userapp_session_target(&fixture.state, "sid-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(target.binding.app_id, APP_ID);
    assert_eq!(target.binding.project_id, "cid-a");
    assert_eq!(target.container_info.unwrap().project_id, APP_ID);
    let target = resolve_agent_target(
        &fixture.state,
        Some("another-user"),
        Some("cid-a"),
        Some("sid-a"),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(target.container_info.is_none());
}

#[tokio::test]
async fn stale_application_lifecycle_is_known_missing_and_never_uses_computer_fallback() {
    let fixture = Fixture::new().await;
    fixture
        .state
        .projects
        .accept_userapp_agent_binding(APP_ID, "previous-lifecycle", "old-cid", USER_ID)
        .await
        .unwrap();
    let (status, body) = post_body(
        &controls(fixture.state.clone()),
        "/status",
        json!({"user_id": USER_ID, "project_id": "old-cid"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["is_alive"], false);
    assert!(fixture.runtime.lookups.lock().unwrap().is_empty());
    assert!(fixture.agent.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn status_of_retained_old_sid_does_not_report_the_replacement_agent_as_alive() {
    let fixture = Fixture::new().await;
    fixture.bind("cid-a", "sid-a1").await;
    fixture.bind("cid-a", "sid-a2").await;
    fixture.bind("cid-b", "sid-b").await;
    let router = controls(fixture.state.clone());

    // Membership of the old SID remains for history/SSE, but its process has exited.
    assert!(
        fixture
            .state
            .projects
            .get_userapp_agent_session("sid-a1")
            .await
            .unwrap()
            .is_some()
    );
    let (status, body) = post_body(
        &router,
        "/status",
        json!({
            "user_id": USER_ID, "project_id": "cid-a", "session_id": "sid-a1"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["project_id"], "cid-a");
    assert_eq!(body["data"]["is_alive"], false);
    assert!(body["data"]["session_id"].is_null());

    // The existing Java request shape still queries the current project agent.
    let (_, body) = post_body(
        &router,
        "/status",
        json!({
            "user_id": USER_ID, "project_id": "cid-a"
        }),
    )
    .await;
    assert_eq!(body["data"]["is_alive"], true);
    assert_eq!(body["data"]["session_id"], "sid-a2");
    let (_, body) = post_body(
        &router,
        "/status",
        json!({
            "user_id": USER_ID, "project_id": "cid-a", "session_id": "sid-a2"
        }),
    )
    .await;
    assert_eq!(body["data"]["is_alive"], true);
    assert_eq!(body["data"]["session_id"], "sid-a2");

    let calls = fixture.agent.calls.lock().unwrap();
    assert!(calls.iter().any(|call| matches!(call, AgentCall::Status(req) if req.project_id == "cid-a" && req.session_id == "sid-a1")));
    assert!(calls.iter().any(|call| matches!(call, AgentCall::Status(req) if req.project_id == "cid-a" && req.session_id.is_empty())));
    assert!(calls.iter().any(|call| matches!(call, AgentCall::Status(req) if req.project_id == "cid-a" && req.session_id == "sid-a2")));
    assert_eq!(
        fixture
            .agent
            .sessions
            .lock()
            .unwrap()
            .get("cid-a")
            .map(String::as_str),
        Some("sid-a2")
    );
    assert_eq!(
        fixture
            .agent
            .sessions
            .lock()
            .unwrap()
            .get("cid-b")
            .map(String::as_str),
        Some("sid-b")
    );
}

#[tokio::test]
async fn dropping_the_fixed_port_fixture_releases_the_listener_before_the_next_fixture() {
    let first = Fixture::new().await;
    assert_eq!(
        first
            .runtime
            .builder
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .internal_port,
        60000
    );
    drop(first);
    let probe = std::net::TcpListener::bind((
        std::net::Ipv4Addr::LOCALHOST,
        shared_types::GRPC_DEFAULT_PORT,
    ))
    .unwrap();
    drop(probe);
    let second = Fixture::new().await;
    second.bind("replacement-fixture", "replacement-sid").await;
    let (_, body) = post_body(
        &controls(second.state.clone()),
        "/status",
        json!({
            "user_id": USER_ID, "project_id": "replacement-fixture"
        }),
    )
    .await;
    assert_eq!(body["data"]["is_alive"], true);
    assert_eq!(body["data"]["session_id"], "replacement-sid");
}
