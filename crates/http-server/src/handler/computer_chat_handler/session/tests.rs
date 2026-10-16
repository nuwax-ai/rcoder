//! Registration protocol tests do not execute or synthesize model output.
use super::*;
use axum::{body::to_bytes, response::IntoResponse};

fn project() -> ProjectAndContainerInfo {
    let mut project = ProjectAndContainerInfo::new("completion-project".into());
    project.set_user_id(Some("completion-user".into()));
    project.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
    project.set_container(Some(ContainerBasicInfo {
        container_id: "captured-pod".into(),
        container_name: "captured-workload".into(),
        container_ip: "127.0.0.1".into(),
        internal_port: shared_types::GRPC_DEFAULT_PORT,
        external_port: 0,
        project_id: "completion-user".into(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: format!("http://127.0.0.1:{}", shared_types::GRPC_DEFAULT_PORT),
        workload_uid: Some("captured-sts".into()),
    }));
    project
}
fn accepted(sid: &str) -> HttpResult<ChatResponse> {
    HttpResult::success(
        serde_json::from_value(serde_json::json!({
            "project_id": "completion-project", "session_id": sid,
            "request_id": "accepted-request", "error": null,
        }))
        .unwrap(),
    )
}
async fn wire(result: HttpResult<ChatResponse>, status: axum::http::StatusCode, sid: &str) {
    // HttpResult retains the established HTTP-200 error envelope contract.
    // The expected failure classification remains asserted independently.
    assert_eq!(shared_types::error_status_from_code(&result.code), status);
    let response = result.into_response();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let data: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 64).await.unwrap()).unwrap();
    assert_eq!(data["data"]["session_id"], sid);
    assert_eq!(data["error_detail"]["task_id"], sid);
    assert_eq!(data["error_detail"]["stage"], "chat.session_registration");
    assert_eq!(data["success"], false);
}

#[test]
fn dispatch_snapshot_detaches_mutable_shared_container_entry() {
    let registered = project();
    let captured = capture_dispatch_project(&registered).unwrap();
    let mut replacement = registered.container_info().unwrap();
    replacement.container_id = "replacement-pod".into();
    replacement.workload_uid = Some("replacement-sts".into());
    registered
        .container()
        .unwrap()
        .update(replacement, shared_types::ServiceType::ComputerAgentRunner);
    assert_eq!(
        registered.container_info().unwrap().container_id,
        "replacement-pod"
    );
    assert_eq!(
        captured.container_info().unwrap().container_id,
        "captured-pod"
    );
    assert_eq!(
        captured.persistence_identity().generation,
        registered.persistence_identity().generation
    );
}

#[tokio::test]
async fn committed_completion_preserves_success_without_extra_lookup() {
    let mut result = accepted("committed-session");
    complete_session_registration(
        &mut result,
        &project(),
        "committed-session",
        Ok(PersistenceWriteOutcome::Committed),
        async { panic!("unexpected readback") },
        "zh-CN",
    )
    .await;
    assert!(result.is_success());
    assert!(result.error_detail.is_none());
}

#[tokio::test]
async fn deferred_same_identity_is_confirmed_by_one_readback() {
    let captured = project();
    let reads = std::sync::atomic::AtomicUsize::new(0);
    let mut result = accepted("deferred-session");
    complete_session_registration(
        &mut result,
        &captured,
        "deferred-session",
        Ok(PersistenceWriteOutcome::Deferred {
            reason: "commit acknowledgement unavailable".into(),
        }),
        async {
            reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(true)
        },
        "zh-CN",
    )
    .await;
    assert!(result.is_success());
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn deferred_unknown_registration_returns_503_with_accepted_session() {
    let mut result = accepted("deferred-missing");
    complete_session_registration(
        &mut result,
        &project(),
        "deferred-missing",
        Ok(PersistenceWriteOutcome::Deferred {
            reason: "controlled database outage".into(),
        }),
        async { Ok(false) },
        "zh-CN",
    )
    .await;
    assert!(!result.is_success());
    let detail = result.error_detail.as_ref().unwrap();
    assert!(!detail.retryable, "a client must not retry the prompt");
    assert!(detail.hint.contains("不要重复"));
    wire(
        result,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "deferred-missing",
    )
    .await;
}

#[tokio::test]
async fn deferred_readback_error_remains_unavailable_instead_of_404() {
    let mut result = accepted("deferred-read-error");
    complete_session_registration(
        &mut result,
        &project(),
        "deferred-read-error",
        Ok(PersistenceWriteOutcome::Deferred {
            reason: "commit unconfirmed".into(),
        }),
        async { anyhow::bail!("controlled readback unavailable") },
        "en-US",
    )
    .await;
    wire(
        result,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "deferred-read-error",
    )
    .await;
}

#[tokio::test]
async fn superseded_completion_returns_409_without_binding_to_new_executor() {
    let mut result = accepted("old-executor-session");
    complete_session_registration(
        &mut result,
        &project(),
        "old-executor-session",
        Ok(PersistenceWriteOutcome::Superseded),
        async { panic!("unexpected readback") },
        "zh-CN",
    )
    .await;
    wire(
        result,
        axum::http::StatusCode::CONFLICT,
        "old-executor-session",
    )
    .await;
}

#[tokio::test]
async fn append_failure_preserves_session_and_never_claims_success() {
    let mut result = accepted("append-failed");
    complete_session_registration(
        &mut result,
        &project(),
        "append-failed",
        Err(anyhow::anyhow!("controlled append failure")),
        async { panic!("unexpected readback") },
        "en-US",
    )
    .await;
    wire(
        result,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "append-failed",
    )
    .await;
}

#[tokio::test]
async fn registration_failure_preserves_existing_runner_business_error() {
    let mut result = accepted("runner-failed-session");
    result.code = shared_types::ERR_INVALID_PARAMS.into();
    result.success = false;
    result.message = "original runner failure".into();
    result.data.as_mut().unwrap().error = Some("original runner failure".into());
    complete_session_registration(
        &mut result,
        &project(),
        "runner-failed-session",
        Err(anyhow::anyhow!("controlled append failure")),
        async { panic!("unexpected readback") },
        "en-US",
    )
    .await;
    assert_eq!(result.code, shared_types::ERR_INVALID_PARAMS);
    assert_eq!(result.message, "original runner failure");
    assert_eq!(
        result.data.as_ref().unwrap().error.as_deref(),
        Some("original runner failure")
    );
    assert!(!result.is_success());
}

#[tokio::test]
async fn optimistic_local_session_does_not_confirm_a_deferred_registration() {
    let captured = project();
    let mut local_pending = captured.clone();
    local_pending.add_session("ambiguous-session");
    assert!(local_pending.sessions().contains("ambiguous-session"));
    assert_eq!(
        captured.persistence_identity().generation,
        local_pending.persistence_identity().generation
    );
    let mut result = accepted("ambiguous-session");
    complete_session_registration(
        &mut result,
        &captured,
        "ambiguous-session",
        Ok(PersistenceWriteOutcome::Deferred {
            reason: "commit unconfirmed".into(),
        }),
        async { Ok(false) },
        "en-US",
    )
    .await;
    wire(
        result,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "ambiguous-session",
    )
    .await;
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn accepted_tonic_chat_is_not_dispatched_again_after_registration_failure() {
    use crate::handler::computer_chat_handler::forward::{
        ComputerForwardParams, forward_computer_request_to_container,
    };
    use crate::handler::userapp_agent_target::test_support::{AgentCall, Fixture};
    let fixture = Fixture::new().await;
    let request: ComputerChatRequest = serde_json::from_value(serde_json::json!({
        "user_id": "completion-user", "project_id": "completion-project",
        "prompt": "protocol-only dispatch", "request_id": "accepted-once",
    }))
    .unwrap();
    let captured = project();
    let basic = captured.container_info().unwrap();
    #[cfg(feature = "deploy-host")]
    shared_types::published::register_direct(
        &basic.container_name,
        std::net::Ipv4Addr::LOCALHOST.into(),
    );
    let mut result = forward_computer_request_to_container(ComputerForwardParams {
        request: &request,
        project_id: captured.project_id(),
        work_dir_id: "/tmp/protocol-only",
        container_info: &basic,
        grpc_pool: &fixture.state.grpc_pool,
        locale: "en-US",
        is_devcomputer: false,
        namespace: &fixture.state.config.app_manager.namespace,
        cluster_domain: &fixture.state.cluster_domain,
        runtime: fixture.state.runtime(),
        service_type: shared_types::ServiceType::ComputerAgentRunner,
        diagnostic_identifier: "completion-user".into(),
        force_new_session: false,
    })
    .await;
    assert!(
        result.is_success(),
        "controlled gRPC acceptance failed: {}",
        result.message
    );
    let sid = result.data.as_ref().unwrap().session_id.clone();
    complete_session_registration(
        &mut result,
        &captured,
        &sid,
        Err(anyhow::anyhow!(
            "controlled registration failure after tonic acceptance"
        )),
        async { panic!("unexpected readback") },
        "en-US",
    )
    .await;
    assert_eq!(
        fixture
            .agent
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| matches!(call, AgentCall::Chat(_)))
            .count(),
        1
    );
    wire(result, axum::http::StatusCode::SERVICE_UNAVAILABLE, &sid).await;
}

#[cfg(feature = "userapp-turso")]
#[path = "http_protocol.rs"]
mod http_protocol;
