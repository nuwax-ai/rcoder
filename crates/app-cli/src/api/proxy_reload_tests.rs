use super::*;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

fn state(root: &Path) -> AppState {
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    let server = Arc::new(crate::server::ServerState::new(
        crate::runtime_status::RuntimeStatusService::default(),
    ));
    server.mark_initialized();
    server.set_release(toml::from_str("schema_version=1\nrelease_id='reload-release'\nworkspace_name='reload-test'\nminimum_app_cli_version='0.0.1'\nruntime_image_digest='fixture'\nservices=[]\n[pingap]\nmode='managed'\nversion='0.15.0'\ncommit='8270a1ebb7a238ea86fa220215714613410378bb'\n").unwrap());
    server.set_proxy_context(root.join("workspace"), true);
    AppState {
        readiness: Arc::new(crate::business_readiness::BusinessReadinessObserver::new(
            server.clone(),
        )),
        server,
        pingap_bin: root.join("never-spawn-this-proxy"),
        log_dir: root.join("logs"),
    }
}

async fn call(
    state: &AppState,
    method: &str,
    path: &str,
    id: &str,
) -> (StatusCode, serde_json::Value) {
    call_optional_id(state, method, path, Some(id)).await
}

async fn call_optional_id(
    state: &AppState,
    method: &str,
    path: &str,
    id: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(id) = id {
        request = request.header("X-Request-ID", id);
    }
    let response = super::super::api_router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    (status, body)
}

#[tokio::test]
async fn explicit_head_keeps_preflight_and_failed_request_visible_without_header() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path());
    let mut claim = Claim::admit(
        &state,
        "new-reload".into(),
        RequestMetadata::capture(&state).unwrap(),
    )
    .unwrap();
    let (status, body) = call_optional_id(&state, "GET", "/v1/proxy/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["operation"]["operation_id"], "new-reload");
    assert_eq!(body["data"]["operation"]["state"], "pending");
    claim
        .failed(
            &anyhow::anyhow!("candidate failed; previous snapshot restored"),
            false,
        )
        .unwrap();
    let previous = crate::proxy::compiler::CompileOutcome {
        config_path: directory.path().join("previous.toml"),
        expected_hash: "previous-hash".into(),
        publication_id: uuid::Uuid::new_v4().to_string(),
        operation_id: Some("old-operation".into()),
        serving_release_id: Some("old-release".into()),
        config_digest: "a".repeat(64),
        entry_probes: vec![],
        business_probes: vec![],
    };
    crate::proxy::compiler::record_official_phase(
        &previous,
        ProxyPhase::Serving,
        Some(&uuid::Uuid::new_v4().to_string()),
    )
    .unwrap();
    let (status, body) = call_optional_id(&state, "GET", "/v1/proxy/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"]["confirmation"]["operation_id"],
        "old-operation"
    );
    assert_eq!(
        body["data"]["confirmation"]["confirmed"], false,
        "synthetic old serving view has no physical ownership proof"
    );
    assert_eq!(body["data"]["operation"]["operation_id"], "new-reload");
    assert_eq!(body["data"]["operation"]["state"], "failed");
    let mut next = Claim::admit(
        &state,
        "next-reload".into(),
        RequestMetadata::capture(&state).unwrap(),
    )
    .unwrap();
    next.failed(&anyhow::anyhow!("next candidate invalid"), false)
        .unwrap();
    assert_eq!(current_id(&state).unwrap().as_deref(), Some("next-reload"));
    let (status, body) = call(&state, "POST", "/v1/proxy/reload", "new-reload").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("previous snapshot restored")
    );
    assert_eq!(
        current_id(&state).unwrap().as_deref(),
        Some("next-reload"),
        "replay cannot replace the explicit current request head"
    );
}

#[tokio::test]
async fn head_directory_failure_records_failure_before_any_proxy_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path());
    let journal = root(&state).unwrap();
    std::fs::create_dir_all(journal.join("head.json")).unwrap();
    let (status, body) = call(&state, "POST", "/v1/proxy/reload", "head-fails").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["data"]["state"], "failed");
    assert_eq!(body["data"]["operation_id"], "head-fails");
    assert!(body["message"].as_str().unwrap().contains("reload head"));
    let record = read(&journal, "head-fails").unwrap().unwrap();
    assert_eq!(record.operation.state, ProxyReloadState::Failed);
    assert!(record.candidate.is_none());
    assert!(!state.pingap_bin.exists());
    let mut writer = state.server.begin_auxiliary_write().unwrap();
    writer.confirm();
    drop(writer);
    let (retry_status, retry) = call(&state, "POST", "/v1/proxy/reload", "head-fails").await;
    assert_eq!(retry_status, status);
    assert_eq!(retry["message"], body["message"]);
    // Remove only the deliberately-created test obstruction, never evidence.
    std::fs::remove_dir(journal.join("head.json")).unwrap();
    let (status, body) = call(&state, "POST", "/v1/proxy/reload", "after-head-fixed").await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "workspace has no service in this component fixture"
    );
    assert_eq!(body["data"]["operation_id"], "after-head-fixed");
    assert_eq!(
        current_id(&state).unwrap().as_deref(),
        Some("after-head-fixed")
    );
    assert_eq!(
        read(&journal, "head-fails")
            .unwrap()
            .unwrap()
            .operation
            .state,
        ProxyReloadState::Failed
    );
}

#[test]
fn request_identity_is_bounded_and_cannot_escape_its_journal() {
    let mut headers = HeaderMap::new();
    assert!(uuid::Uuid::parse_str(&request_id(&headers).unwrap()).is_ok());
    for invalid in ["", "two words", "\t", &"a".repeat(129)] {
        headers.insert("X-Request-ID", invalid.parse().unwrap());
        assert!(requested_id(&headers).is_err());
    }
    headers.insert("X-Request-ID", "../../outside".parse().unwrap());
    let id = request_id(&headers).unwrap();
    let root = Path::new("/private/reload-operations");
    let path = record_path(root, &id);
    assert_eq!(path.parent(), Some(root));
    assert_eq!(path.file_name().unwrap().to_string_lossy().len(), 69);
    headers.append("X-Request-ID", "duplicate".parse().unwrap());
    assert!(requested_id(&headers).is_err());
}

#[tokio::test]
async fn router_replays_original_success_and_reports_metadata_conflict() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path());
    let mut claim = Claim::admit(
        &state,
        "repeat-me".into(),
        RequestMetadata::capture(&state).unwrap(),
    )
    .unwrap();
    let mut data = ProxyReloadData::pending(&claim.record.operation);
    data.reloaded = true;
    data.state = ProxyReloadState::Succeeded;
    data.publication_id = "original-frozen-publication".into();
    data.config_digest = "original-canonical-sha".into();
    data.confirmation = Some(shared_types::ProxyConfirmation {
        contract: shared_types::ProxyConfirmationContract::OfficialRestartHttpV1,
        confirmed: true,
        phase: ProxyPhase::Serving,
        operation_id: "repeat-me".into(),
        proxy_instance_id: Some(uuid::Uuid::new_v4().to_string()),
        config_digest: Some(data.config_digest.clone()),
    });
    claim.succeeded(data).unwrap();
    let (status, body) = call(&state, "POST", "/v1/proxy/reload", "repeat-me").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"]["publication_id"],
        "original-frozen-publication"
    );
    assert_eq!(body["data"]["verified"], false);
    assert_eq!(body["data"]["process_instance_id"], "");
    assert_eq!(body["data"]["replayed"], true);
    assert_eq!(body["data"]["confirmation"]["confirmed"], true);
    assert!(
        !state.pingap_bin.exists(),
        "replay must not need or spawn the proxy binary"
    );
    state
        .server
        .set_proxy_context(directory.path().join("different-workspace"), true);
    let (status, body) = call(&state, "POST", "/v1/proxy/reload", "repeat-me").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "ERR_OPERATION_ID_CONFLICT");
    assert_eq!(body["data"]["operation_id"], "repeat-me");
}

#[tokio::test]
async fn router_replays_failed_request_and_query_keeps_its_terminal() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path());
    let mut claim = Claim::admit(
        &state,
        "failed-request".into(),
        RequestMetadata::capture(&state).unwrap(),
    )
    .unwrap();
    claim
        .failed(
            &anyhow::anyhow!("native plugin constructor rejected credentials"),
            false,
        )
        .unwrap();
    let (status, body) = call(&state, "POST", "/v1/proxy/reload", "failed-request").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["data"]["state"], "failed");
    assert_eq!(body["data"]["operation_id"], "failed-request");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("native plugin constructor")
    );
    let (status, body) = call(&state, "GET", "/v1/proxy/status", "failed-request").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["applied"], false);
    assert_eq!(body["data"]["operation"]["state"], "failed");
    assert_eq!(body["data"]["operation"]["operation_id"], "failed-request");
}

#[tokio::test]
async fn unfinished_old_reload_is_queryable_but_does_not_block_a_new_identity() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path());
    let mut claim = Claim::admit(
        &state,
        "interrupted-request".into(),
        RequestMetadata::capture(&state).unwrap(),
    )
    .unwrap();
    claim.record.owner_id = "exited-owner".into();
    write(&claim.root, &claim.record, false).unwrap();
    // Simulate a process crash that cannot run a Rust Drop callback.
    claim.completed = true;
    let (status, body) = call(&state, "POST", "/v1/proxy/reload", "interrupted-request").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["data"]["state"], "recovery_required");
    // Check owner-bound observation directly and preserve the original record.
    let old = read(&root(&state).unwrap(), "interrupted-request")
        .unwrap()
        .unwrap();
    assert_eq!(
        old.observed_operation(&state).state,
        ProxyReloadState::RecoveryRequired
    );
    let (status, body) = call(&state, "POST", "/v1/proxy/reload", "new-request").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["data"]["operation_id"], "new-request");
    assert_eq!(
        read(&root(&state).unwrap(), "new-request")
            .unwrap()
            .unwrap()
            .operation
            .state,
        ProxyReloadState::Failed
    );
}

#[test]
fn interrupted_admission_records_unknown_without_faking_success() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path());
    let claim = Claim::admit(
        &state,
        "cancelled-request".into(),
        RequestMetadata::capture(&state).unwrap(),
    )
    .unwrap();
    drop(claim);
    let record = read(&root(&state).unwrap(), "cancelled-request")
        .unwrap()
        .unwrap();
    assert_eq!(record.operation.state, ProxyReloadState::RecoveryRequired);
    assert!(record.result.is_none());
    assert!(
        record
            .operation
            .error_message
            .unwrap()
            .contains("terminal result")
    );
}

#[tokio::test]
async fn corrupted_success_record_cannot_replay_success_or_fence_new_work() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path());
    let mut claim = Claim::admit(
        &state,
        "corrupt-success".into(),
        RequestMetadata::capture(&state).unwrap(),
    )
    .unwrap();
    claim.record.operation.state = ProxyReloadState::Succeeded;
    write(&claim.root, &claim.record, false).unwrap();
    claim.completed = true;
    for method in ["POST", "GET"] {
        let endpoint = if method == "POST" {
            "/v1/proxy/reload"
        } else {
            "/v1/proxy/status"
        };
        let (status, body) = call(&state, method, endpoint, "corrupt-success").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["success"], false);
        assert_eq!(body["code"], "ERR_RECOVERY_REQUIRED");
        assert_eq!(body["data"]["operation_id"], "corrupt-success");
        assert!(!state.pingap_bin.exists());
    }
    let mut writer = state.server.begin_auxiliary_write().unwrap();
    writer.confirm();
}

#[tokio::test]
async fn same_pending_request_is_observed_while_another_identity_is_busy() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(directory.path());
    let mut claim = Claim::admit(
        &state,
        "active-request".into(),
        RequestMetadata::capture(&state).unwrap(),
    )
    .unwrap();
    let _gate = gate().unwrap();
    let (status, body) = call(&state, "POST", "/v1/proxy/reload", "active-request").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["data"]["operation_id"], "active-request");
    assert_eq!(body["data"]["state"], "pending");
    let (status, body) = call(&state, "POST", "/v1/proxy/reload", "other-request").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "ERR_OPERATION_IN_PROGRESS");
    assert!(
        read(&root(&state).unwrap(), "other-request")
            .unwrap()
            .is_none()
    );
    claim
        .failed(&anyhow::anyhow!("fixture cancelled before mutation"), false)
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_http_handler_does_not_drop_owned_reload_execution() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir().unwrap();
    let mut state = state(directory.path());
    let workspace = directory.path().join("workspace");
    std::fs::write(
        workspace.join("proxy.toml"),
        "[servers.app]\naddr='0.0.0.0:9080'\n",
    )
    .unwrap();
    let mut release = state.server.release().unwrap();
    release.pingap.mode = workspace_manifest::PingapMode::Custom;
    release.pingap.config = Some("proxy.toml".into());
    state.server.set_release(release);
    let marker = directory.path().join("capability-worker-pid");
    let binary = directory.path().join("delayed-official-basic-fixture");
    std::fs::write(&binary, format!(r#"#!/usr/bin/env python3
import http.server, json, os, pathlib, sys, time
if '-t' in sys.argv: sys.exit(0)
pathlib.Path({marker}).write_text(str(os.getpid()))
time.sleep(1)
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({{'pid':str(os.getpid()), 'version':'0.15.0','git_hash':'8270a1ebb7a238ea86fa220215714613410378bb','supported_plugins':['mock']}}).encode()
        self.send_response(200); self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
    def log_message(self, *args): pass
address = os.environ['PINGAP_ADMIN_ADDR'].split(':')
http.server.HTTPServer((address[0],int(address[1])), Handler).serve_forever()
"#, marker=serde_json::to_string(&marker.to_string_lossy()).unwrap())).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    state.pingap_bin = binary;
    let caller_state = state.clone();
    let caller = tokio::spawn(async move {
        call(
            &caller_state,
            "POST",
            "/v1/proxy/reload",
            "detached-request",
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !marker.exists() {
            assert!(
                !caller.is_finished(),
                "preflight must run before cancelling the HTTP handler"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let record = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let record = read(&root(&state).unwrap(), "detached-request")
                .unwrap()
                .unwrap();
            if record.operation.state == ProxyReloadState::Failed {
                break record;
            }
            assert_ne!(
                record.operation.state,
                ProxyReloadState::RecoveryRequired,
                "dropping the caller must not interrupt the owned operation"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        record
            .operation
            .error_message
            .unwrap()
            .contains("owner capability"),
        "component fixture reaches the real host admission failure, after completing preflight"
    );
    let pid: u32 = std::fs::read_to_string(marker).unwrap().parse().unwrap();
    assert!(
        !process_utils::process_exists(pid).unwrap(),
        "temporary process must be reaped"
    );
    let mut writer = state
        .server
        .begin_auxiliary_write()
        .expect("known failed host admission must release its writer");
    writer.confirm();
}
