//! Actual ensure persistence must compare the incoming request's ownership.
use super::*;
use crate::handler::userapp_agent_target::test_support::Fixture;
use shared_types::ProjectAndContainerInfo;

const PROJECT: &str = "ensure-identity-project";
fn basic() -> ContainerBasicInfo {
    ContainerBasicInfo {
        container_id: "original-container".into(),
        container_name: "original-workload".into(),
        container_ip: "127.0.0.1".into(),
        internal_port: shared_types::GRPC_DEFAULT_PORT,
        external_port: 0,
        project_id: "original-owner".into(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: "http://127.0.0.1".into(),
        workload_uid: None,
    }
}
async fn rejects_mismatched_request(user: &str, pod: Option<&str>, original: ServiceType) {
    let fixture = Fixture::new().await;
    let mut registered = ProjectAndContainerInfo::new(PROJECT.into());
    registered.set_user_id(Some("original-owner".into()));
    registered.set_pod_id(Some("original-pod".into()));
    registered.set_service_type(Some(original));
    registered.set_container(Some(basic()));
    fixture
        .state
        .insert_project(PROJECT.into(), Arc::new(registered))
        .unwrap();
    let before = fixture.state.get_project(PROJECT).unwrap();
    let request: EnsurePodRequest = serde_json::from_value(serde_json::json!({
        "user_id": user, "project_id": PROJECT, "pod_id": pod,
        "service_type": "computer-agent-runner",
    }))
    .unwrap();
    let mut replacement = basic();
    replacement.container_id = "different-request-container".into();
    replacement.container_name = "different-request-workload".into();
    replacement.project_id = pod.unwrap_or(user).to_owned();
    let result = persist_and_respond(
        &fixture.state,
        &request,
        &ServiceType::ComputerAgentRunner,
        &replacement,
        false,
        "existing".into(),
    )
    .await;
    assert!(
        result.is_err(),
        "ensure reused historical owner fields and accepted another request identity"
    );
    let response = axum::response::IntoResponse::into_response(result.unwrap_err());
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap(),
    )
    .unwrap();
    let reason = if user != "original-owner" {
        shared_types::error_codes::COMPUTER_OWNER_USER_MISMATCH
    } else if pod != Some("original-pod") {
        shared_types::error_codes::COMPUTER_OWNER_POD_MISMATCH
    } else {
        shared_types::error_codes::COMPUTER_OWNER_SERVICE_FAMILY_MISMATCH
    };
    assert_eq!(body["code"], shared_types::ERR_COMPUTER_OWNER_CONFLICT);
    assert_eq!(body["success"], false);
    assert_eq!(body["error_detail"]["reason_code"], reason);
    assert_eq!(
        body["error_detail"]["stage"],
        "computer.registration.before_dispatch"
    );
    assert_eq!(body["error_detail"]["retryable"], false);
    assert!(body["data"].is_null());
    assert!(body.get("operation_id").is_none());
    assert!(fixture.agent.calls.lock().unwrap().is_empty());
    let wire = body.to_string();
    for private in [
        "original-owner",
        "original-pod",
        "original-container",
        "original-workload",
    ] {
        assert!(!wire.contains(private), "owner values must remain internal");
    }
    let current = fixture.state.get_project(PROJECT).unwrap();
    assert_eq!(
        current.container_info().unwrap().container_id,
        "original-container"
    );
    assert_eq!(current.user_id(), Some("original-owner"));
    assert_eq!(current.pod_id(), Some("original-pod"));
    assert_eq!(
        current.service_type(),
        Some(original.family_representative())
    );
    assert_eq!(
        current.persistence_identity().generation,
        before.persistence_identity().generation
    );
}

#[tokio::test]
async fn pod_ensure_rejects_a_different_user_without_rebinding_the_original_project() {
    rejects_mismatched_request(
        "another-owner",
        Some("original-pod"),
        ServiceType::ComputerAgentRunner,
    )
    .await;
}
#[tokio::test]
async fn pod_ensure_rejects_a_different_shared_pod_without_rebinding_the_original_project() {
    rejects_mismatched_request(
        "original-owner",
        Some("another-pod"),
        ServiceType::ComputerAgentRunner,
    )
    .await;
}
#[tokio::test]
async fn pod_ensure_cannot_move_a_web_project_into_the_computer_service_family() {
    rejects_mismatched_request(
        "original-owner",
        Some("original-pod"),
        ServiceType::WebAgentRunner,
    )
    .await;
}

#[tokio::test]
async fn compute_only_metadata_intent_keeps_newer_chat_fields_and_chat_none_can_clear_them() {
    use crate::app_state::RegistrationMetadata;
    let fixture = Fixture::new().await;
    let mut initial = ProjectAndContainerInfo::new("metadata-intent".into());
    initial.set_user_id(Some("original-owner".into()));
    initial.set_service_type(Some(ServiceType::ComputerAgentRunner));
    initial.set_container(Some(basic()));
    initial.set_request_id(Some("old-request".into()));
    fixture
        .state
        .insert_project("metadata-intent".into(), Arc::new(initial))
        .unwrap();
    let stale = fixture.state.get_project("metadata-intent").unwrap();
    let mut latest = (*stale).clone();
    latest.set_request_id(Some("newer-chat-request".into()));
    latest.set_model_provider(Some(shared_types::ModelProviderConfig {
        id: "newer-model".into(),
        name: "protocol fixture".into(),
        base_url: "https://model.invalid".into(),
        api_key: "test-only-not-a-secret".into(),
        requires_openai_auth: false,
        default_model: "fixture".into(),
        api_protocol: None,
        wire_api: None,
    }));
    fixture
        .state
        .insert_project("metadata-intent".into(), Arc::new(latest))
        .unwrap();
    let options = ContainerCreateOptions {
        user_id: "original-owner".into(),
        project_id: "metadata-intent".into(),
        resource_limits: None,
        pod_id: None,
        isolation_type: None,
        tenant_id: None,
        space_id: None,
        service_type: ServiceType::ComputerAgentRunner,
    };
    let params = ComputerContainerManager::creation_params(&options).unwrap();
    let preserved = fixture
        .state
        .ensure_computer_project_mapping(&params, stale, RegistrationMetadata::PreserveChatMetadata)
        .await
        .unwrap();
    assert_eq!(preserved.request_id(), Some("newer-chat-request"));
    assert_eq!(preserved.model_provider().unwrap().id, "newer-model");
    // Explicitly absent chat fields still mean update/clear, not preserve.
    let mut explicit_clear = (*preserved).clone();
    explicit_clear.set_request_id(None);
    explicit_clear.set_model_provider(None);
    let updated = fixture
        .state
        .ensure_computer_project_mapping(
            &params,
            Arc::new(explicit_clear),
            RegistrationMetadata::UpdateChatMetadata,
        )
        .await
        .unwrap();
    assert_eq!(updated.request_id(), None);
    assert!(updated.model_provider().is_none());
}

#[path = "central_workload_tests.rs"]
mod central_workload_tests;
