//! A new explicit artifact is not a request to replay an unavailable old one.
use super::*;
use http_body_util::BodyExt as _;
use std::{path::PathBuf, sync::atomic::Ordering};

const URL: &str = "https://artifact.example.test/current.zip";

fn obsolete_recipe(args: &RuntimeArgs, state: &ServerState) -> (PathBuf, Vec<u8>) {
    {
        let mut slot = state.journal.lock().unwrap();
        let journal = slot.as_mut().unwrap();
        let mut receipt = journal.receipt.clone().unwrap();
        for request in [
            &mut receipt.request,
            receipt.active.as_mut().unwrap().request.as_mut().unwrap(),
        ] {
            request.execution_target = None;
            request.local_path = Some(args.workspace.join("builds/obsolete.zip"));
        }
        journal.write(receipt).unwrap();
    }
    assert!(restored_runtime_args_inner(args, state, false).is_err());
    assert!(state.source_replacement_hold_only());
    let saved = state
        .journal
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .preserve_source_history()
        .unwrap();
    let bytes = std::fs::read(&saved).unwrap();
    (saved, bytes)
}

async fn history_fixture() -> (
    tempfile::TempDir,
    RuntimeArgs,
    Arc<ServerState>,
    PathBuf,
    Vec<u8>,
) {
    let (dir, args, state) = fixture(ExecutionTarget::ProjectRun).await;
    // The shared fixture begins with a separate current-credential hold. This
    // case isolates the real missing historical recipe's diagnostic hold.
    state.runtime_recovery_hold.store(0, Ordering::Release);
    let (saved, bytes) = obsolete_recipe(&args, &state);
    assert_eq!(
        state.runtime_recovery_hold.load(Ordering::Acquire),
        SOURCE_HISTORY_HOLD
    );
    (dir, args, state, saved, bytes)
}

async fn submit_new_artifact(
    args: &RuntimeArgs,
    state: &Arc<ServerState>,
    operation_id: &str,
    pg: Option<shared_types::StartPgCredential>,
) -> axum::response::Response {
    let kernel = state.runtime_kernel().unwrap();
    let identity = kernel.identity();
    let request = shared_types::RuntimeOperationRequest {
        operation_id: operation_id.into(),
        expected_runtime_instance_id: identity.runtime_instance_id.clone(),
        expected_revision: kernel.status().await.unwrap().revision,
        workspace_id: identity.workspace_id.clone(),
        kind: shared_types::RuntimeOperationKind::Deploy,
        profile: shared_types::RunProfileInput::Artifact {
            artifact: shared_types::ArtifactInput::Url {
                url: URL.into(),
                sha256: Some("ab".repeat(32)),
            },
        },
        run_config: pg.map(|pg| shared_types::OperationRunConfig { pg: Some(pg) }),
        request_context: None,
    };
    crate::api::bound_router(
        args.workspace.clone(),
        args.log_dir.clone(),
        args.pingap_bin.clone(),
        state.clone(),
    )
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/v1/runtime/operations")
            .header("content-type", "application/json")
            .header("x-deploy-token", state.control_token().unwrap())
            .body(Body::from(serde_json::to_vec(&request).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn dispatched_artifact(state: &ServerState) -> DeployRequest {
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.deploy_rx.lock().await.recv(),
    )
    .await
    .expect("new artifact must pass the admitted worker's preparation policy")
    .expect("artifact preparation channel remains open")
}

#[tokio::test]
async fn fresh_artifact_deploy_after_obsolete_history_admits_and_dispatches_new_url() {
    let (_dir, args, state, saved, bytes) = history_fixture().await;
    let response = submit_new_artifact(&args, &state, "new-artifact", None).await;
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        axum::http::StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let request = dispatched_artifact(&state).await;
    assert_eq!(
        request.runtime_operation_id.as_deref(),
        Some("new-artifact")
    );
    assert_eq!(request.url, URL);
    assert_eq!(request.sha256.as_deref(), Some("ab".repeat(32).as_str()));
    assert!(!state.runtime_recovery_hold_active());
    assert_eq!(std::fs::read(saved).unwrap(), bytes);
}

#[tokio::test]
async fn fresh_artifact_deploy_after_obsolete_history_accepts_legacy_deploy_api_input() {
    let (_dir, args, state, saved, bytes) = history_fixture().await;
    let response = crate::api::bound_router(
        args.workspace,
        args.log_dir,
        args.pingap_bin,
        state.clone(),
    )
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/v1/deploy")
            .header("content-type", "application/json")
            .header("x-deploy-token", state.control_token().unwrap())
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "url":URL,"sha256":"ab".repeat(32),"release_id":"explicit-current-release",
                    "operation_id":"explicit-current-deploy"
                }))
                .unwrap(),
            ))
            .unwrap(),
    )
    .await
    .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        axum::http::StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let request = dispatched_artifact(&state).await;
    assert_eq!(request.url, URL);
    assert_eq!(request.release_id, "explicit-current-release");
    assert!(!state.runtime_recovery_hold_active());
    assert_eq!(std::fs::read(saved).unwrap(), bytes);
}

#[tokio::test]
async fn new_artifact_still_requires_current_pg_when_credentials_are_held() {
    let (_dir, args, state, _saved, _bytes) = history_fixture().await;
    state.begin_credentials_hold();
    assert_eq!(
        submit_new_artifact(&args, &state, "missing-current-pg", None)
            .await
            .status(),
        axum::http::StatusCode::CONFLICT
    );
    assert!(
        state
            .runtime_kernel()
            .unwrap()
            .get("missing-current-pg")
            .await
            .unwrap()
            .is_none()
    );
    let response = submit_new_artifact(
        &args,
        &state,
        "current-pg-artifact",
        Some(shared_types::StartPgCredential {
            username: "dev".into(),
            password: "current-private-credential".into(),
        }),
    )
    .await;
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let request = dispatched_artifact(&state).await;
    assert_eq!(
        request.run_pg.unwrap().password,
        "current-private-credential"
    );
    assert!(!state.runtime_recovery_hold_active());
}

#[tokio::test]
async fn old_artifact_restore_without_a_real_recipe_remains_refused() {
    let (_dir, _args, state, _saved, _bytes) = history_fixture().await;
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-private-credential".into(),
    };
    assert!(
        state
            .credential_recovery(
                Some(&pg),
                CredentialRecoveryPurpose::RestoreConfirmedArtifact
            )
            .is_err()
    );
    assert!(state.runtime_recovery_hold_active());
}

#[tokio::test]
async fn new_artifact_cannot_bypass_unknown_outcome_or_unconfirmed_cleanup() {
    for cleanup in [false, true] {
        let (_dir, args, state, _saved, _bytes) = history_fixture().await;
        if cleanup {
            state.shutdown_unconfirmed.store(true, Ordering::Release);
        } else {
            state.begin_runtime_recovery_hold();
        }
        assert_eq!(
            submit_new_artifact(&args, &state, "protected-new-artifact", None)
                .await
                .status(),
            axum::http::StatusCode::CONFLICT
        );
        assert!(
            state
                .runtime_kernel()
                .unwrap()
                .get("protected-new-artifact")
                .await
                .unwrap()
                .is_none()
        );
        assert!(state.deploy_rx.lock().await.try_recv().is_err());
        assert!(state.runtime_recovery_hold_active());
    }
}

#[tokio::test]
async fn new_artifact_cannot_bypass_an_actual_auxiliary_writer() {
    let (_dir, args, state) = fixture(ExecutionTarget::ProjectRun).await;
    state.runtime_recovery_hold.store(0, Ordering::Release);
    let mut writer = state.begin_auxiliary_write().unwrap();
    obsolete_recipe(&args, &state);
    assert_eq!(
        submit_new_artifact(&args, &state, "writer-protected-artifact", None)
            .await
            .status(),
        axum::http::StatusCode::CONFLICT
    );
    assert!(
        state
            .runtime_kernel()
            .unwrap()
            .get("writer-protected-artifact")
            .await
            .unwrap()
            .is_none()
    );
    assert!(state.deploy_rx.lock().await.try_recv().is_err());
    writer.confirm();
}
