use std::sync::Arc;

use axum::{body::Body, http::Request, http::StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use shared_types::{RunProfileInput, RuntimeOperationKind, RuntimeOperationRequest};
use tower::ServiceExt;

use super::{KernelStartupStatus, ServerState, publish_runtime_kernel};
use crate::runtime_kernel::{RuntimeKernel, RuntimeStore};
use crate::runtime_status::RuntimeStatusService;

fn kernel(root: &std::path::Path) -> Arc<RuntimeKernel> {
    let source = root.join("workspace");
    std::fs::create_dir_all(&source).unwrap();
    let store = RuntimeStore::open_with_root(root.join("control"), &source).unwrap();
    let identity = store
        .load_or_init_identity(
            "startup-app".into(),
            "userapp-dev".into(),
            "workspace".into(),
            source.to_string_lossy().into_owned(),
            "startup-generation".into(),
        )
        .unwrap();
    Arc::new(RuntimeKernel::new(store, identity, Box::new(|_| {})))
}

async fn query(
    root: &std::path::Path,
    state: &Arc<ServerState>,
    path: &str,
    request: Option<RuntimeOperationRequest>,
    token: &str,
) -> (StatusCode, Value) {
    let router = crate::api::bound_router(
        root.join("workspace"),
        root.join("logs"),
        root.join("unused-pingap"),
        state.clone(),
    );
    let method = if request.is_some() { "POST" } else { "GET" };
    let body = request
        .map(|request| Body::from(serde_json::to_vec(&request).unwrap()))
        .unwrap_or_else(Body::empty);
    let response = router
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("Content-Type", "application/json")
                .header("X-Deploy-Token", token)
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn request(kernel: &RuntimeKernel, kind: RuntimeOperationKind) -> RuntimeOperationRequest {
    RuntimeOperationRequest {
        operation_id: format!("startup-{kind:?}"),
        expected_runtime_instance_id: kernel.identity().runtime_instance_id.clone(),
        expected_revision: 0,
        workspace_id: "workspace".into(),
        kind,
        profile: RunProfileInput::Source {
            workspace_id: "workspace".into(),
        },
        run_config: None,
        request_context: None,
    }
}

#[tokio::test]
async fn assembly_and_credential_failures_report_recovery_without_accepting_operations() {
    for credential_failure in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(ServerState::new(RuntimeStatusService::default()));
        state.initialize_owner_token().unwrap();
        state.mark_kernel_required();
        let candidate = kernel(root.path());
        let assembled = if credential_failure {
            // Real credential persistence fails atomically against a directory.
            std::fs::create_dir(root.path().join("control/token")).unwrap();
            Ok(candidate.clone())
        } else {
            let blocked = root.path().join("blocked-state-root");
            std::fs::write(&blocked, "retained sentinel").unwrap();
            RuntimeStore::open_with_root(blocked, &root.path().join("workspace"))
                .map(|_| candidate.clone())
        };
        let error = match publish_runtime_kernel(&state, assembled) {
            Ok(_) => panic!("a real assembly/credential failure must not publish the kernel"),
            Err(error) => error,
        };
        if credential_failure {
            assert!(format!("{error:#}").contains("publish owner control credential"));
            assert!(root.path().join("control/token").is_dir());
        } else {
            assert_eq!(
                std::fs::read_to_string(root.path().join("blocked-state-root")).unwrap(),
                "retained sentinel"
            );
        }
        assert!(matches!(
            state.kernel_startup_status(),
            KernelStartupStatus::Failed
        ));
        assert!(state.initializing());
        assert!(state.runtime_kernel().is_none());
        state.mark_kernel_required(); // A later declaration cannot erase failure.
        let token = state.control_token().unwrap();
        for path in [
            "/v1/runtime/identity",
            "/v1/runtime/status",
            "/v1/runtime/recovery",
        ] {
            let (status, body) = query(root.path(), &state, path, None, &token).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}: {body}");
            assert_eq!(body["code"], "ERR_RECOVERY_REQUIRED", "{path}: {body}");
        }
        for kind in [RuntimeOperationKind::Start, RuntimeOperationKind::Stop] {
            let (status, body) = query(
                root.path(),
                &state,
                "/v1/runtime/operations",
                Some(request(&candidate, kind)),
                &token,
            )
            .await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{kind:?}: {body}");
            assert_eq!(body["code"], "ERR_RECOVERY_REQUIRED");
        }
        assert!(
            candidate
                .status()
                .await
                .unwrap()
                .active_operation_id
                .is_none()
        );
        assert_eq!(
            std::fs::read_dir(root.path().join("control/operations"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(state.pending_deploy_count().await, 0);
        let (status, body) = query(
            root.path(),
            &state,
            "/v1/runtime/recovery",
            None,
            "foreign-owner-token",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "ERR_FORBIDDEN");
    }
}

#[tokio::test]
async fn ready_kernel_keeps_identity_and_precise_stop_during_business_recovery() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(ServerState::new(RuntimeStatusService::default()));
    state.initialize_owner_token().unwrap();
    state.mark_kernel_required();
    let published = publish_runtime_kernel(&state, Ok(kernel(root.path()))).unwrap();
    assert!(matches!(
        state.kernel_startup_status(),
        KernelStartupStatus::Ready(_)
    ));
    assert!(state.initializing());
    state.begin_runtime_recovery_hold();
    state.mark_kernel_failed(); // Later business failures cannot remove identity.
    assert!(matches!(
        state.kernel_startup_status(),
        KernelStartupStatus::Ready(_)
    ));
    let token = state.control_token().unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("control/token")).unwrap(),
        token
    );
    for path in ["/v1/runtime/identity", "/v1/runtime/status"] {
        let (status, body) = query(root.path(), &state, path, None, &token).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(
            body["data"]["runtime_instance_id"],
            published.identity().runtime_instance_id
        );
    }
    let (status, body) = query(
        root.path(),
        &state,
        "/v1/runtime/operations",
        Some(request(&published, RuntimeOperationKind::Start)),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "ERR_INVALID_STATE");
    assert!(
        published
            .status()
            .await
            .unwrap()
            .active_operation_id
            .is_none()
    );
    let (status, body) = query(
        root.path(),
        &state,
        "/v1/runtime/operations",
        Some(request(&published, RuntimeOperationKind::Stop)),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    // Stop records its own pending identity rather than taking the executing
    // business slot. Acceptance must not pretend physical cleanup completed.
    assert_eq!(body["data"]["operation_id"], "startup-Stop");
    let stopped = published.get("startup-Stop").await.unwrap().unwrap();
    assert_eq!(stopped.state, shared_types::RuntimeOperationState::Accepted);
    assert_eq!(
        published.outstanding_stop_operations().unwrap(),
        vec!["startup-Stop"]
    );
    assert_eq!(
        published.status().await.unwrap().desired,
        shared_types::DesiredState::Stopped
    );
    assert!(
        published
            .status()
            .await
            .unwrap()
            .active_operation_id
            .is_none()
    );
    assert!(state.runtime_recovery_hold_active());
}

#[tokio::test]
async fn terminal_kernel_startup_transition_rejects_late_and_duplicate_publication() {
    let root = tempfile::tempdir().unwrap();
    let candidate = kernel(root.path());
    let failed = Arc::new(ServerState::new(RuntimeStatusService::default()));
    failed.initialize_owner_token().unwrap();
    failed.mark_kernel_required();
    // Hold the shared transition lock as a deterministic publication barrier.
    // A late setter has begun, but cannot bypass the failure winner.
    let late_publication = {
        let mut transition = failed.kernel_startup.write().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let late_state = failed.clone();
        let late_kernel = candidate.clone();
        let publication = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            late_state.set_runtime_kernel(late_kernel)
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let _previous = std::mem::replace(&mut *transition, KernelStartupStatus::Failed);
        publication
    };
    assert!(!late_publication.join().unwrap());
    assert!(failed.runtime_kernel().is_none());
    assert!(publish_runtime_kernel(&failed, Ok(candidate.clone())).is_err());
    assert!(!root.path().join("control/token").exists());
    let (status, body) = query(root.path(), &failed, "/v1/runtime/identity", None, "").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "ERR_RECOVERY_REQUIRED");

    let ready = Arc::new(ServerState::new(RuntimeStatusService::default()));
    ready.initialize_owner_token().unwrap();
    ready.mark_kernel_required();
    let original = publish_runtime_kernel(&ready, Ok(candidate)).unwrap();
    let saved_token = std::fs::read(root.path().join("control/token")).unwrap();
    let second = tempfile::tempdir().unwrap();
    let replacement = kernel(second.path());
    assert!(!ready.set_runtime_kernel(replacement.clone()));
    assert!(publish_runtime_kernel(&ready, Ok(replacement)).is_err());
    ready.mark_kernel_failed();
    ready.mark_kernel_required();
    assert!(Arc::ptr_eq(&ready.runtime_kernel().unwrap(), &original));
    assert_eq!(
        std::fs::read(root.path().join("control/token")).unwrap(),
        saved_token
    );
    assert!(!second.path().join("control/token").exists());
    let (status, body) = query(root.path(), &ready, "/v1/runtime/identity", None, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"]["runtime_instance_id"],
        original.identity().runtime_instance_id
    );
}

#[tokio::test]
async fn unsupported_mode_does_not_claim_kernel_assembly_or_recovery_failure() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(ServerState::new(RuntimeStatusService::default()));
    state.mark_initialized();
    assert!(matches!(
        state.kernel_startup_status(),
        KernelStartupStatus::Unsupported
    ));
    for path in ["/v1/runtime/identity", "/v1/runtime/status"] {
        let (status, body) = query(root.path(), &state, path, None, "").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "ERR_PROTOCOL_UNSUPPORTED");
    }
}
