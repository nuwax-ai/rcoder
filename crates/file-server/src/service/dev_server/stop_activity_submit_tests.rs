use super::*;
use shared_types::{
    RunProfileInput, RuntimeOperationRequest, RuntimeOperationState, RuntimeOperationView,
};

#[tokio::test]
async fn original_stop_rejects_foreign_view_before_releasing_pending_activity() {
    for mismatch in ["operation", "instance", "kind", "digest"] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::Config::from_env().unwrap();
        config.log_base_dir = dir.path().to_owned();
        let manager = DevServerManager::new(Arc::new(config));
        let original = RuntimeOperationRequest {
            operation_id: "original-stop".into(),
            expected_runtime_instance_id: "captured-owner".into(),
            expected_revision: 7,
            workspace_id: "captured-workspace".into(),
            kind: RuntimeOperationKind::Stop,
            profile: RunProfileInput::Source {
                workspace_id: "captured-workspace".into(),
            },
            run_config: None,
            request_context: None,
        };
        let posts = Arc::new(Mutex::new(Vec::<RuntimeOperationRequest>::new()));
        let captured = posts.clone();
        let observed = posts.clone();
        let router = axum::Router::new()
            .route("/v1/runtime/operations", axum::routing::post(move |axum::Json(request): axum::Json<RuntimeOperationRequest>| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().push(request.clone());
                    (axum::http::StatusCode::ACCEPTED, axum::Json(serde_json::json!({"data":shared_types::RuntimeOperationAccepted { operation_id:request.operation_id.clone(), state:RuntimeOperationState::Accepted, poll:format!("/v1/runtime/operations/{}",request.operation_id) }})))
                }
            }))
            .route("/v1/runtime/operations/{id}", axum::routing::get(move || {
                let observed = observed.clone();
                async move {
                    let request = observed.lock().unwrap()[0].clone();
                    let mut view = RuntimeOperationView {
                        operation_id:request.operation_id.clone(), runtime_instance_id:request.expected_runtime_instance_id.clone(),kind:request.kind,
                        state:RuntimeOperationState::Succeeded,request_digest:shared_types::runtime_request_digest(&request).unwrap(),revision:8,
                        error_code:None,error_message:None,failure_detail:None,
                    };
                    match mismatch {
                        "operation" => view.operation_id = "foreign-operation".into(),
                        "instance" => view.runtime_instance_id = "foreign-owner".into(),
                        "kind" => view.kind = RuntimeOperationKind::Restart,
                        "digest" => view.request_digest = "foreign-request".into(),
                        _ => unreachable!(),
                    }
                    axum::Json(serde_json::json!({"data":view}))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let external = crate::models::ExternalOwner {
            address: address.clone(),
            token: "test-token".into(),
            runtime_instance_id: original.expected_runtime_instance_id.clone(),
        };
        let intent = manager
            .prepare_external_intent("project", dir.path(), &external, &original)
            .unwrap();
        // This simulates a real original Stop sent by the previous SDK, whose
        // stored intent did not yet contain the new dispatch flag.
        let record = Arc::new(Mutex::new(Vec::new()));
        let result = STOP_ACTIVITY
            .scope(record.clone(), async {
                prepare_external("project", &original, &address, dir.path(), None).unwrap();
                external_dispatched(&original).unwrap();
                reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .unwrap()
                    .post(format!("http://{address}/v1/runtime/operations"))
                    .header("X-Deploy-Token", "test-token")
                    .json(&original)
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
                let client = owner_client::OwnerClient::new(&address, "test-token").unwrap();
                manager
                    .resume_external_intent_until(
                        "project",
                        &client,
                        &intent,
                        &original,
                        tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                    )
                    .await
            })
            .await;
        server.abort();
        assert!(
            result.is_err(),
            "{mismatch}: foreign receipt must be rejected"
        );
        assert_eq!(
            posts.lock().unwrap().len(),
            1,
            "the observer cannot send another Stop"
        );
        assert_eq!(
            record
                .lock()
                .unwrap()
                .iter()
                .filter(|activity| activity.pending)
                .count(),
            1,
            "{mismatch}: an unverified terminal view cannot release the captured Stop activity"
        );
    }
}
