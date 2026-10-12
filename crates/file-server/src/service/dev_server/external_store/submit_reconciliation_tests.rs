use super::*;
use shared_types::{RunProfileInput, RuntimeOperationState};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn manager(root: &Path) -> DevServerManager {
    let mut config = crate::Config::from_env().unwrap();
    config.log_base_dir = root.to_owned();
    DevServerManager::new(Arc::new(config))
}
fn request() -> RuntimeOperationRequest {
    RuntimeOperationRequest {
        operation_id: "original-operation".into(),
        expected_runtime_instance_id: "original-owner".into(),
        expected_revision: 7,
        workspace_id: "original-workspace".into(),
        kind: RuntimeOperationKind::Deploy,
        profile: RunProfileInput::Artifact {
            artifact: shared_types::ArtifactInput::ArtifactId {
                artifact_id: "original-release".into(),
            },
        },
        run_config: None,
        request_context: Some("original-task".into()),
    }
}
fn owner(address: &str) -> ExternalOwner {
    ExternalOwner {
        address: address.into(),
        token: "test-token".into(),
        runtime_instance_id: "original-owner".into(),
    }
}
#[derive(Default)]
struct Wire {
    posts: Vec<RuntimeOperationRequest>,
    queried: Vec<String>,
    visible: Option<RuntimeOperationState>,
    lost_ack: bool,
    hidden_polls: usize,
    mismatch: Option<&'static str>,
    rejected: Option<&'static str>,
    unconfirmed_reply: Option<&'static str>,
    get_delay: Duration,
    slow_query_once: bool,
    identity_source: Option<std::path::PathBuf>,
}

async fn fixture(
    root: &Path,
    wire: Wire,
) -> (String, Arc<Mutex<Wire>>, tokio::task::JoinHandle<()>) {
    use axum::response::IntoResponse as _;
    let state = Arc::new(Mutex::new(wire));
    let posted = state.clone();
    let queried = state.clone();
    let identified = state.clone();
    let identity_source = root.to_path_buf();
    let root = root.to_path_buf();
    let router = axum::Router::new()
        .route("/v1/runtime/identity", axum::routing::get(move || {
            let identified = identified.clone();
            let default_source = identity_source.clone();
            async move {
                let source = identified.lock().unwrap().identity_source.clone().unwrap_or(default_source);
                axum::Json(serde_json::json!({"code":"OK","message":"ok","data":shared_types::RuntimeIdentityView {
                    application_id:std::env::var("PROJECT_ID").ok().filter(|v|!v.trim().is_empty()).unwrap_or_else(||"unknown-app".into()),
                    service_family:"userapp-dev".into(),workspace_id:"original-workspace".into(),source_root:source.to_string_lossy().into_owned(),
                    runtime_instance_id:"original-owner".into(),deployment_generation_id:"captured-generation".into(),
                    protocol_version:shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,capabilities:vec![shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY.into()],
                }}))
            }
        }))
        .route("/v1/runtime/operations", axum::routing::post(move |headers: axum::http::HeaderMap, axum::Json(request): axum::Json<RuntimeOperationRequest>| {
            let posted = posted.clone();
            let root = root.clone();
            async move {
                assert_eq!(headers["x-deploy-token"], "test-token");
                let disk = read(&root.join("dev-server-external.json")).unwrap();
                assert!(disk.intents.values().any(|intent| intent.request.operation_id == request.operation_id && intent.dispatch_pending), "original dispatch identity must be durable before any wire side effect");
                let (lost_ack, rejected, unconfirmed_reply) = {
                    let mut wire = posted.lock().unwrap();
                    wire.posts.push(request.clone());
                    (wire.lost_ack, wire.rejected, wire.unconfirmed_reply)
                };
                if let Some(code) = rejected {
                    return (axum::http::StatusCode::CONFLICT, axum::Json(serde_json::json!({"code":code,"message":"known rejection"}))).into_response();
                }
                match unconfirmed_reply {
                    Some("malformed") => return (axum::http::StatusCode::OK,"truncated-ack").into_response(),
                    Some("server-error") => return (axum::http::StatusCode::INTERNAL_SERVER_ERROR,axum::Json(serde_json::json!({"code":"ERR_BACKEND_ERROR","message":"unconfirmed write"}))).into_response(),
                    Some("foreign-receipt") => return (axum::http::StatusCode::ACCEPTED,axum::Json(serde_json::json!({"data":shared_types::RuntimeOperationAccepted { operation_id:"foreign-operation".into(),state:RuntimeOperationState::Accepted,poll:"/v1/runtime/operations/foreign-operation".into() }}))).into_response(),
                    None => {}
                    Some(other) => panic!("invalid fixture {other}"),
                }
                if lost_ack { std::future::pending::<()>().await; }
                (axum::http::StatusCode::ACCEPTED, axum::Json(serde_json::json!({"data":shared_types::RuntimeOperationAccepted {
                    operation_id: request.operation_id.clone(), state: RuntimeOperationState::Accepted, poll: format!("/v1/runtime/operations/{}", request.operation_id),
                }}))).into_response()
            }
        }))
        .route("/v1/runtime/operations/{id}", axum::routing::get(move |headers: axum::http::HeaderMap, axum::extract::Path(id): axum::extract::Path<String>| {
            let queried = queried.clone();
            async move {
                assert_eq!(headers["x-deploy-token"], "test-token");
                let (view, delay) = {
                    let mut wire = queried.lock().unwrap();
                    wire.queried.push(id.clone());
                    let original = wire.posts.iter().find(|request| request.operation_id == id).cloned();
                    let state = if original.is_some() && wire.hidden_polls > 0 {
                        wire.hidden_polls -= 1;
                        None
                    } else { wire.visible };
                    let view = original.zip(state).map(|(original, state)| {
                        let mut view = RuntimeOperationView {
                            operation_id: original.operation_id.clone(),
                            runtime_instance_id: original.expected_runtime_instance_id.clone(),
                            kind: original.kind, state, revision: 8,
                            request_digest: shared_types::runtime_request_digest(&original).unwrap(),
                            error_code: if state == RuntimeOperationState::Failed { Some("ACTUAL_START_FAILED".into()) } else { None },
                            error_message: if state == RuntimeOperationState::Failed { Some("real startup failure".into()) } else { None },
                            failure_detail: None,
                        };
                        match wire.mismatch {
                            Some("instance") => view.runtime_instance_id = "foreign-owner".into(),
                            Some("operation") => view.operation_id = "different-operation".into(),
                            Some("kind") => view.kind = RuntimeOperationKind::Restart,
                            Some("digest") => view.request_digest = "different-content".into(),
                            None => {}
                            Some(other) => panic!("invalid fixture: {other}"),
                        }
                        view
                    });
                    let delay = if view.is_some() && wire.slow_query_once {
                        wire.slow_query_once = false;
                        Duration::from_millis(400)
                    } else { wire.get_delay };
                    (view, delay)
                };
                tokio::time::sleep(delay).await;
                match view {
                    Some(view) => axum::Json(serde_json::json!({"data":view})).into_response(),
                    None => axum::http::StatusCode::NOT_FOUND.into_response(),
                }
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address, state, server)
}
fn client(address: &str) -> OwnerClient {
    OwnerClient::with_request_timeout(address, "test-token", Duration::from_millis(150)).unwrap()
}
fn deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(2)
}

#[tokio::test]
async fn timeout_then_transient_404_only_reads_original_digest_and_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let (address, wire, server) = fixture(
        dir.path(),
        Wire {
            visible: Some(RuntimeOperationState::Succeeded),
            lost_ack: true,
            hidden_polls: 2,
            ..Wire::default()
        },
    )
    .await;
    let manager = manager(dir.path());
    let original = request();
    let intent = manager
        .prepare_external_intent("project", dir.path(), &owner(&address), &original)
        .unwrap();
    let view = manager
        .resume_external_intent_until("project", &client(&address), &intent, &original, deadline())
        .await
        .unwrap();
    assert_eq!(view.state, RuntimeOperationState::Succeeded);
    assert_eq!(
        view.request_digest,
        shared_types::runtime_request_digest(&original).unwrap()
    );
    let wire = wire.lock().unwrap();
    assert_eq!(wire.posts.len(), 1);
    assert!(wire.queried.len() >= 4);
    assert!(wire.queried.iter().all(|id| id == &original.operation_id));
    assert_eq!(
        serde_json::to_value(&wire.posts[0]).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    server.abort();
}

#[tokio::test]
async fn normal_ack_then_first_query_timeout_reconciles_original_within_budget() {
    normal_ack_query_case(true, 0).await;
}

#[tokio::test]
async fn normal_ack_then_transient_404_reconciles_original_within_budget() {
    normal_ack_query_case(false, 2).await;
}

async fn normal_ack_query_case(slow_query_once: bool, hidden_polls: usize) {
    let dir = tempfile::tempdir().unwrap();
    let (address, wire, server) = fixture(
        dir.path(),
        Wire {
            visible: Some(RuntimeOperationState::Succeeded),
            slow_query_once,
            hidden_polls,
            ..Wire::default()
        },
    )
    .await;
    let manager = manager(dir.path());
    let original = request();
    let intent = manager
        .prepare_external_intent("project", dir.path(), &owner(&address), &original)
        .unwrap();
    let result = manager
        .resume_external_intent_until("project", &client(&address), &intent, &original, deadline())
        .await;
    server.abort();
    let view = result.expect("a confirmed POST acknowledgement must retain the original observation budget across transient GET failure");
    assert_eq!(view.operation_id, original.operation_id);
    assert_eq!(
        view.request_digest,
        shared_types::runtime_request_digest(&original).unwrap()
    );
    assert_eq!(wire.lock().unwrap().posts.len(), 1);
}

#[tokio::test]
async fn timeout_reconciliation_rejects_foreign_identity_and_changed_payload() {
    for mismatch in ["instance", "operation", "kind", "digest"] {
        let dir = tempfile::tempdir().unwrap();
        let (address, wire, server) = fixture(
            dir.path(),
            Wire {
                visible: Some(RuntimeOperationState::Succeeded),
                lost_ack: true,
                mismatch: Some(mismatch),
                ..Wire::default()
            },
        )
        .await;
        let manager = manager(dir.path());
        let original = request();
        let intent = manager
            .prepare_external_intent("project", dir.path(), &owner(&address), &original)
            .unwrap();
        let error = manager
            .resume_external_intent_until(
                "project",
                &client(&address),
                &intent,
                &original,
                deadline(),
            )
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("identity or digest mismatch"),
            "{mismatch}: {error:#}"
        );
        assert!(
            manager.read_external_state().unwrap().intents[&key("project", original.kind)]
                .dispatch_pending
        );
        assert_eq!(wire.lock().unwrap().posts.len(), 1);
        server.abort();
    }
}

#[tokio::test]
async fn timeout_reconciliation_returns_real_failure_without_claiming_success() {
    let dir = tempfile::tempdir().unwrap();
    let (address, wire, server) = fixture(
        dir.path(),
        Wire {
            visible: Some(RuntimeOperationState::Failed),
            lost_ack: true,
            ..Wire::default()
        },
    )
    .await;
    let manager = manager(dir.path());
    let original = request();
    let intent = manager
        .prepare_external_intent("project", dir.path(), &owner(&address), &original)
        .unwrap();
    let view = manager
        .resume_external_intent_until("project", &client(&address), &intent, &original, deadline())
        .await
        .unwrap();
    assert_eq!(view.state, RuntimeOperationState::Failed);
    assert_eq!(view.error_code.as_deref(), Some("ACTUAL_START_FAILED"));
    assert_eq!(view.error_message.as_deref(), Some("real startup failure"));
    assert_eq!(wire.lock().unwrap().posts.len(), 1);
    server.abort();
}

#[tokio::test]
async fn exhausted_deadline_retains_unknown_intent_and_next_observation_never_reposts() {
    let dir = tempfile::tempdir().unwrap();
    let (address, wire, server) = fixture(
        dir.path(),
        Wire {
            lost_ack: true,
            ..Wire::default()
        },
    )
    .await;
    let first = manager(dir.path());
    let original = request();
    let external = owner(&address);
    let intent = first
        .prepare_external_intent("project", dir.path(), &external, &original)
        .unwrap();
    let now = tokio::time::Instant::now();
    let error = first
        .resume_external_intent_until(
            "project",
            &client(&address),
            &intent,
            &original,
            now + Duration::from_millis(600),
        )
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("result remains unknown"),
        "{error:#}"
    );
    assert!(now.elapsed() < Duration::from_millis(1000));
    assert!(
        first.read_external_state().unwrap().intents[&key("project", original.kind)]
            .dispatch_pending
    );
    drop(first);
    let restarted = manager(dir.path());
    let mut later = original.clone();
    later.operation_id = "unused-new-id".into();
    later.expected_revision = 100;
    let recovered = restarted
        .prepare_external_intent("project", dir.path(), &external, &later)
        .unwrap();
    assert_eq!(recovered.request.operation_id, original.operation_id);
    let error = restarted
        .resume_external_intent_until(
            "project",
            &client(&address),
            &recovered,
            &later,
            tokio::time::Instant::now() + Duration::from_millis(100),
        )
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("result remains unknown"));
    assert_eq!(
        wire.lock().unwrap().posts.len(),
        1,
        "404 during recovery must not repeat a dispatched write"
    );
    wire.lock().unwrap().visible = Some(RuntimeOperationState::Succeeded);
    let view = restarted
        .resume_external_intent_until("project", &client(&address), &recovered, &later, deadline())
        .await
        .unwrap();
    assert_eq!(view.operation_id, original.operation_id);
    assert_eq!(view.state, RuntimeOperationState::Succeeded);
    assert_eq!(wire.lock().unwrap().posts.len(), 1);
    restarted
        .finish_external_intent("project", &recovered.request, false)
        .unwrap();
    assert!(restarted.read_external_state().unwrap().intents.is_empty());
    server.abort();
}

#[tokio::test]
async fn unresolved_transport_history_does_not_permanently_block_a_new_run() {
    let dir = tempfile::tempdir().unwrap();
    let (address, wire, server) = fixture(
        dir.path(),
        Wire {
            lost_ack: true,
            ..Wire::default()
        },
    )
    .await;
    let manager = manager(dir.path());
    let original = request();
    let external = owner(&address);
    let intent = manager
        .prepare_external_intent("project", dir.path(), &external, &original)
        .unwrap();
    assert!(
        manager
            .resume_external_intent_until(
                "project",
                &client(&address),
                &intent,
                &original,
                tokio::time::Instant::now() + Duration::from_millis(600)
            )
            .await
            .is_err()
    );
    assert_eq!(
        wire.lock().unwrap().posts.len(),
        1,
        "the first write must actually have reached this fixture before testing unresolved-history recovery"
    );
    manager.ensure_new_build_admissible("project").unwrap();
    let mut next = original.clone();
    next.operation_id = "next-run".into();
    next.request_context = Some("new-task".into());
    next.profile = RunProfileInput::Artifact {
        artifact: shared_types::ArtifactInput::ArtifactId {
            artifact_id: "corrected-release".into(),
        },
    };
    {
        let mut wire = wire.lock().unwrap();
        wire.visible = Some(RuntimeOperationState::Succeeded);
        wire.lost_ack = false;
    }
    let next_intent = manager
        .prepare_external_intent("project", dir.path(), &external, &next)
        .unwrap();
    let view = manager
        .resume_external_intent_until(
            "project",
            &client(&address),
            &next_intent,
            &next,
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(view.operation_id, next.operation_id);
    let state = manager.read_external_state().unwrap();
    assert!(
        state.completed["project|original-operation"].dispatch_pending,
        "keep unresolved original history without inventing a terminal result"
    );
    assert_eq!(
        state.intents[&key("project", next.kind)]
            .request
            .operation_id,
        next.operation_id
    );
    assert_eq!(
        wire.lock()
            .unwrap()
            .posts
            .iter()
            .map(|request| request.operation_id.as_str())
            .collect::<Vec<_>>(),
        ["original-operation", "next-run"]
    );
    server.abort();
}

#[tokio::test]
async fn verified_preadmission_rejection_remains_retryable_without_fake_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let (address, wire, server) = fixture(
        dir.path(),
        Wire {
            rejected: Some(shared_types::ERR_REVISION_MISMATCH),
            ..Wire::default()
        },
    )
    .await;
    let manager = manager(dir.path());
    let original = request();
    let external = owner(&address);
    let intent = manager
        .prepare_external_intent("project", dir.path(), &external, &original)
        .unwrap();
    let error = manager
        .resume_external_intent_until("project", &client(&address), &intent, &original, deadline())
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<super::super::owner_client::SubmissionRejected>()
            .unwrap()
            .proves_not_admitted()
    );
    assert!(manager.read_external_state().unwrap().intents.is_empty());
    assert_eq!(
        wire.lock().unwrap().queried.len(),
        1,
        "real rejection must not be overwritten by a subsequent GET"
    );
    {
        let mut wire = wire.lock().unwrap();
        wire.rejected = None;
        wire.visible = Some(RuntimeOperationState::Succeeded);
    }
    let mut corrected = original.clone();
    corrected.operation_id = "corrected-request".into();
    corrected.expected_revision = 8;
    let corrected_intent = manager
        .prepare_external_intent("project", dir.path(), &external, &corrected)
        .unwrap();
    assert_eq!(
        manager
            .resume_external_intent_until(
                "project",
                &client(&address),
                &corrected_intent,
                &corrected,
                deadline()
            )
            .await
            .unwrap()
            .operation_id,
        corrected.operation_id
    );
    assert_eq!(wire.lock().unwrap().posts.len(), 2);
    server.abort();
}

#[tokio::test]
async fn deadline_exhausted_before_dispatch_does_not_mark_or_write_the_intent() {
    let dir = tempfile::tempdir().unwrap();
    let (address, wire, server) = fixture(
        dir.path(),
        Wire {
            get_delay: Duration::from_millis(200),
            ..Wire::default()
        },
    )
    .await;
    let manager = manager(dir.path());
    let original = request();
    let intent = manager
        .prepare_external_intent("project", dir.path(), &owner(&address), &original)
        .unwrap();
    // The ordinary 10s SDK cannot replace the much shorter inherited budget.
    let owner = OwnerClient::new(&address, "test-token").unwrap();
    let start = tokio::time::Instant::now();
    assert!(
        manager
            .resume_external_intent_until(
                "project",
                &owner,
                &intent,
                &original,
                start + Duration::from_millis(30)
            )
            .await
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_millis(150));
    assert!(wire.lock().unwrap().posts.is_empty());
    assert!(
        !manager.read_external_state().unwrap().intents[&key("project", original.kind)]
            .dispatch_pending
    );
    server.abort();
}

#[test]
fn legacy_intent_without_dispatch_flag_preserves_original_fields_and_remains_replayable() {
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(dir.path());
    let original = request();
    let intent = manager
        .prepare_external_intent("project", dir.path(), &owner("127.0.0.1:1"), &original)
        .unwrap();
    let mut old = serde_json::to_value(intent).unwrap();
    old.as_object_mut().unwrap().remove("dispatch_pending");
    let old: Intent = serde_json::from_value(old).unwrap();
    assert!(!old.dispatch_pending);
    assert_eq!(old.request.operation_id, original.operation_id);
    assert_eq!(
        old.request.expected_runtime_instance_id,
        original.expected_runtime_instance_id
    );
}

#[tokio::test]
async fn private_config_ack_timeout_and_readonly_restart_recovery_preserve_both_digest_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let app = std::env::var("PROJECT_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "unknown-app".into());
    let token_root = dir.path().join(".app-cli-state").join(app);
    std::fs::create_dir_all(&token_root).unwrap();
    std::fs::write(token_root.join("token"), "test-token").unwrap();
    let (address, wire, server) = fixture(
        dir.path(),
        Wire {
            visible: Some(RuntimeOperationState::Succeeded),
            lost_ack: true,
            identity_source: Some(workspace.clone()),
            ..Wire::default()
        },
    )
    .await;
    let mut original = request();
    original.run_config = Some(shared_types::OperationRunConfig {
        pg: Some(shared_types::StartPgCredential {
            username: "fixture-pg-user".into(),
            password: "fixture-pg-password".into(),
        }),
    });
    let first = manager(dir.path());
    let external = owner(&address);
    let intent = first
        .prepare_external_intent("project", &workspace, &external, &original)
        .unwrap();
    assert!(intent.request.run_config.is_none());
    assert_ne!(
        digest(&original).unwrap(),
        digest(&intent.request).unwrap(),
        "the local transport digest preserves original private configuration"
    );
    assert_eq!(
        shared_types::runtime_request_digest(&original).unwrap(),
        shared_types::runtime_request_digest(&intent.request).unwrap(),
        "the documented native public digest excludes private run configuration"
    );
    let first_view = first
        .resume_external_intent_until("project", &client(&address), &intent, &original, deadline())
        .await
        .unwrap();
    assert_eq!(first_view.state, RuntimeOperationState::Succeeded);
    let bytes = std::fs::read_to_string(first.external_state_path()).unwrap();
    assert!(!bytes.contains("fixture-pg-user") && !bytes.contains("fixture-pg-password"));
    drop(first);
    let restarted = manager(dir.path());
    let mut readonly = original.clone();
    readonly.run_config = None;
    readonly.operation_id = "must-not-refresh-original".into();
    let recovered = restarted
        .prepare_external_intent("project", &workspace, &external, &readonly)
        .unwrap();
    let view = restarted
        .resume_external_intent_until(
            "project",
            &client(&address),
            &recovered,
            &readonly,
            deadline(),
        )
        .await
        .unwrap();
    assert_eq!(view.operation_id, original.operation_id);
    assert_eq!(wire.lock().unwrap().posts.len(), 1);
    restarted
        .finish_external_intent("project", &recovered.request, false)
        .unwrap();
    let (task, view) = restarted
        .recover_external_operation("project", &workspace, &original.operation_id, None)
        .await
        .unwrap();
    assert_eq!(task.as_deref(), Some("original-task"));
    assert_eq!(view.state, RuntimeOperationState::Succeeded);
    assert_eq!(
        view.request_digest,
        shared_types::runtime_request_digest(&original).unwrap()
    );
    assert_eq!(
        wire.lock().unwrap().posts.len(),
        1,
        "readonly recovery never retransmits secrets or another operation"
    );
    server.abort();
}

#[tokio::test]
async fn malformed_ack_with_admitted_original_is_reconciled_in_the_same_call() {
    unconfirmed_reply_case("malformed", true).await;
}

#[tokio::test]
async fn ambiguous_server_error_with_admitted_original_is_reconciled_in_the_same_call() {
    unconfirmed_reply_case("server-error", true).await;
}

#[tokio::test]
async fn foreign_ack_receipt_is_rejected_without_masking_the_protocol_failure() {
    unconfirmed_reply_case("foreign-receipt", false).await;
}

async fn unconfirmed_reply_case(reply: &'static str, should_reconcile: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (address, wire, server) = fixture(
        dir.path(),
        Wire {
            visible: Some(RuntimeOperationState::Succeeded),
            unconfirmed_reply: Some(reply),
            ..Wire::default()
        },
    )
    .await;
    let manager = manager(dir.path());
    let original = request();
    let intent = manager
        .prepare_external_intent("project", dir.path(), &owner(&address), &original)
        .unwrap();
    let result = manager
        .resume_external_intent_until("project", &client(&address), &intent, &original, deadline())
        .await;
    server.abort();
    if should_reconcile {
        let view = result.expect("an unconfirmed reply must not fail an already admitted original operation while its observation budget remains");
        assert_eq!(view.operation_id, original.operation_id);
        assert_eq!(
            view.request_digest,
            shared_types::runtime_request_digest(&original).unwrap()
        );
        assert_eq!(view.state, RuntimeOperationState::Succeeded);
        assert!(wire.lock().unwrap().queried.len() >= 2);
    } else {
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("mismatched operation receipt")
        );
        assert_eq!(wire.lock().unwrap().queried.len(), 1);
    }
    assert_eq!(wire.lock().unwrap().posts.len(), 1);
}
