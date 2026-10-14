use super::computer_agent_progress_notification;
use crate::handler::userapp_agent_target::test_support::{AgentCall, Fixture};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
    routing::get,
};
use shared_types::grpc::ProgressEvent;
use tower::ServiceExt as _;

#[tokio::test]
async fn independent_userapp_sessions_use_the_same_builder_but_separate_http_sse_streams() {
    let fixture = Fixture::new().await;
    #[cfg(feature = "deploy-host")]
    {
        let container = fixture.runtime.builder.lock().unwrap().clone().unwrap();
        let observation =
            shared_types::published::begin_physical_observation(&container.container_id);
        assert!(observation.register(
            &[container.container_name.as_str()],
            container.created_at,
            shared_types::published::Reach::Published {
                ports: std::collections::HashMap::from([(
                    shared_types::GRPC_DEFAULT_PORT,
                    shared_types::GRPC_DEFAULT_PORT,
                )]),
            },
        ));
    }
    assert!(fixture.root.path().exists());
    fixture.bind("conversation-a", "sid-a").await;
    fixture.bind("conversation-b", "sid-b").await;
    for (sid, token) in [("sid-a", "protocol-A"), ("sid-b", "protocol-B")] {
        fixture.agent.events.lock().unwrap().insert(
            sid.into(),
            vec![
                ProgressEvent {
                    message_type: "AgentSessionUpdate".into(),
                    sub_type: "agent_message_chunk".into(),
                    payload: serde_json::json!({"content":{"text":token}}).to_string(),
                    request_id: Some(format!("request-{sid}")),
                    seq: 1,
                    timestamp: 1,
                },
                ProgressEvent {
                    message_type: "SessionPromptEnd".into(),
                    sub_type: "end_turn".into(),
                    payload: serde_json::json!({"reason":"EndTurn"}).to_string(),
                    request_id: Some(format!("request-{sid}")),
                    seq: 2,
                    timestamp: 2,
                },
            ],
        );
    }
    let router = Router::new()
        .route(
            "/computer/progress/{session_id}",
            get(computer_agent_progress_notification),
        )
        .with_state(fixture.state.clone());
    for (sid, own, foreign) in [
        ("sid-a", "protocol-A", "protocol-B"),
        ("sid-b", "protocol-B", "protocol-A"),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/computer/progress/{sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_success());
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .unwrap()
        .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains(own), "own session delivery: {body}");
        assert!(
            !body.contains(foreign),
            "foreign session cannot leak: {body}"
        );
        assert!(body.contains("end_turn"));
    }
    let calls = fixture.agent.calls.lock().unwrap();
    let progress: Vec<_> = calls
        .iter()
        .filter_map(|call| match call {
            AgentCall::Progress(request) => Some(request.session_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(progress, ["sid-a", "sid-b"]);
    assert_eq!(
        fixture
            .runtime
            .writes
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}
