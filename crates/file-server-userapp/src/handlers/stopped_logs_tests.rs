//! Real router counterexamples: app-cli absent must not hide persistent logs.
use std::path::Path;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use userapp_log_reader::{LogCatalog, LogLayout};

use crate::UserAppState;

fn release() -> shared_types::ReleaseLock {
    shared_types::load_release_lock(
        r#"
schema_version=1
release_id='log-fixture'
workspace_name='logs'
minimum_app_cli_version='0.3.9'
runtime_image_digest='fixture'
[pingap]
mode='managed'
version='fixture'
commit='fixture'
[[services]]
service_id='api'
name='api'
dir='api'
type='go'
kind='web'
enabled=true
port=4000
[services.run]
command=['never-execute']
[services.env]
[services.health]
[[services.logs]]
id='application'
glob='application*.log'
format='jsonl'
"#,
    )
    .unwrap()
}

fn fixture(root: &Path, phase: &str) -> (Router, std::path::PathBuf) {
    let source = root.join("workspaces/227");
    let logs = root.join("platform-logs/227/app-cli");
    let state_root = root.join("workspaces/.app-cli-state/227");
    std::fs::create_dir_all(source.join("logs/api")).unwrap();
    std::fs::create_dir_all(logs.join("api")).unwrap();
    std::fs::create_dir_all(&state_root).unwrap();
    std::fs::write(
        logs.join("api/application.log"),
        format!("{{\"message\":\"{phase} service failure\"}}\n"),
    )
    .unwrap();
    std::fs::write(
        logs.join("app-cli.log.2026-10-04"),
        "{\"message\":\"owner startup failed\"}\n",
    )
    .unwrap();
    std::fs::write(
        source.join("logs/api/dev-2026-10-04.log"),
        "compile failed: exit 37\n",
    )
    .unwrap();
    LogCatalog::from_release(
        "227".into(),
        source.clone(),
        logs.clone(),
        release(),
        LogLayout::Builtin,
    )
    .unwrap()
    .publish(&state_root)
    .unwrap();
    // A historical state is observation only; no owner process or port exists.
    std::fs::write(
        state_root.join("old-phase.json"),
        json!({"phase": phase}).to_string(),
    )
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unreachable = port.local_addr().unwrap().to_string();
    drop(port);
    let config = file_server::Config {
        userapp_workspace_dir: root.join("workspaces"),
        log_base_dir: root.join("platform-logs"),
        app_cli_admin_probe_addr: unreachable,
        ..Default::default()
    };
    let fs = file_server::FileServer::builder(config).build().unwrap();
    let router = crate::routes::userapp_top_router()
        .split_for_parts()
        .0
        .with_state(UserAppState::new(fs.state()));
    (router, state_root)
}
async fn post(router: &Router, suffix: &str, body: Value) -> axum::response::Response {
    tokio::time::timeout(
        Duration::from_secs(5),
        router.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/userapp/227/dev/logs/{suffix}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        ),
    )
    .await
    .expect("logs must not wait for management/bootstrap")
    .unwrap()
}
async fn body(response: axum::response::Response) -> Value {
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn not_started_failed_and_stopped_without_owner_keep_logs_and_cursor() {
    for phase in ["not_started", "failed", "stopped"] {
        let root = tempfile::tempdir().unwrap();
        let (router, state_root) = fixture(root.path(), phase);
        let before = std::fs::read(state_root.join("log-catalog.json")).unwrap();
        let sources = body(post(&router, "sources/query", json!({})).await).await;
        assert_eq!(sources["success"], true, "{sources}");
        assert!(
            sources["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["source_id"] == "application")
        );
        let initial = body(post(&router, "query", json!({})).await).await;
        assert_eq!(initial["success"], true, "{initial}");
        let messages = initial["data"]["logs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line["message"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(
            messages
                .iter()
                .any(|message| message.contains("service failure")),
            "{messages:?}"
        );
        assert!(messages.contains(&"owner startup failed"));
        assert!(messages.contains(&"compile failed: exit 37"));
        let next = body(
            post(
                &router,
                "query",
                json!({"cursor": initial["data"]["cursor"]}),
            )
            .await,
        )
        .await;
        assert_eq!(next["data"]["cursor_reset"], false);
        assert!(next["data"]["logs"].as_array().unwrap().is_empty());
        assert_eq!(
            std::fs::read(state_root.join("log-catalog.json")).unwrap(),
            before
        );
        assert!(
            !state_root.join("owner.lock").exists(),
            "query must not acquire ownership"
        );
        assert!(!state_root.join("supervisor.json").exists());
    }
}

#[tokio::test]
async fn malformed_catalog_reports_diagnostic_and_keeps_orchestrator_logs() {
    let root = tempfile::tempdir().unwrap();
    let (router, state_root) = fixture(root.path(), "failed");
    std::fs::write(state_root.join("log-catalog.json"), b"{broken").unwrap();
    let result = body(post(&router, "query", json!({})).await).await;
    assert_eq!(result["success"], true, "{result}");
    assert!(
        result["data"]["source_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["code"] == "catalog_invalid")
    );
    assert!(
        result["data"]["logs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line["message"] == "owner startup failed")
    );
    let sources = body(post(&router, "sources/query", json!({})).await).await;
    assert!(
        sources["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["diagnostic"]["code"] == "catalog_invalid")
    );
    assert_eq!(
        std::fs::read(state_root.join("log-catalog.json")).unwrap(),
        b"{broken"
    );
}

#[tokio::test]
async fn stopped_log_sse_emits_log_and_checkpoint_without_management() {
    let root = tempfile::tempdir().unwrap();
    let (router, state_root) = fixture(root.path(), "stopped");
    let response = post(
        &router,
        "stream",
        json!({"selectors": [{"service_id": "api", "source_ids": ["application"]}]}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut data = response.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(5), data.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let first = String::from_utf8(first.to_vec()).unwrap();
    assert!(
        first.contains("event: log") && first.contains("stopped service failure"),
        "{first}"
    );
    let checkpoint = tokio::time::timeout(Duration::from_secs(5), data.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8(checkpoint.to_vec())
            .unwrap()
            .contains("event: checkpoint")
    );
    drop(data);
    assert!(!state_root.join("owner.lock").exists());
}

#[tokio::test]
async fn catalog_cannot_read_another_application_or_external_directory() {
    let root = tempfile::tempdir().unwrap();
    let (router, state_root) = fixture(root.path(), "stopped");
    let mut catalog: Value =
        serde_json::from_slice(&std::fs::read(state_root.join("log-catalog.json")).unwrap())
            .unwrap();
    catalog["app_id"] = json!("another-app");
    catalog["log_root"] = json!("/etc");
    std::fs::write(state_root.join("log-catalog.json"), catalog.to_string()).unwrap();
    let result = body(post(&router, "query", json!({})).await).await;
    assert!(
        result["data"]["source_errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["code"] == "catalog_invalid")
    );
    assert!(
        result["data"]["logs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|line| line["file"] != "passwd")
    );
}

#[tokio::test]
async fn catalog_release_change_resets_cursor_but_reader_refresh_does_not() {
    let root = tempfile::tempdir().unwrap();
    let (router, state_root) = fixture(root.path(), "stopped");
    let request = json!({"selectors": [{"service_id":"api", "source_ids":["application"]}]});
    let before = body(post(&router, "query", request.clone()).await).await;
    let mut next = release();
    next.release_id = "next-release".into();
    LogCatalog::from_release(
        "227".into(),
        root.path().join("workspaces/227"),
        root.path().join("platform-logs/227/app-cli"),
        next,
        LogLayout::Builtin,
    )
    .unwrap()
    .publish(&state_root)
    .unwrap();
    let changed = body(post(&router, "query", json!({"selectors": [{"service_id":"api", "source_ids":["application"]}], "cursor":before["data"]["cursor"]})).await).await;
    assert_eq!(changed["data"]["cursor_reset"], true);
    let same = body(post(&router, "query", json!({"selectors": [{"service_id":"api", "source_ids":["application"]}], "cursor":changed["data"]["cursor"]})).await).await;
    assert_eq!(same["data"]["cursor_reset"], false);
    assert!(same["data"]["logs"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn direct_single_app_log_route_rejects_foreign_app_without_reading_logs() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("227");
    std::fs::create_dir(&source).unwrap();
    let config = file_server::Config {
        userapp_workspace_dir: source.clone(),
        userapp_single_app_id: Some("227".into()),
        log_base_dir: root.path().join("logs"),
        ..Default::default()
    };
    let fs = file_server::FileServer::builder(config).build().unwrap();
    let router = crate::routes::userapp_top_router()
        .split_for_parts()
        .0
        .with_state(UserAppState::new(fs.state()));
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/userapp/228/dev/logs/query")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let response = body(response).await;
    assert_eq!(response["success"], false);
    assert_eq!(response["code"], shared_types::error_codes::ERR_VALIDATION);
    assert!(response["data"].is_null());
    assert_eq!(std::fs::read_dir(source).unwrap().count(), 0);
    assert!(!root.path().join("logs").exists());
}

#[tokio::test]
async fn only_owner_recovery_file_is_available_through_all_three_log_routes() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("logs/227/app-cli");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(
        logs.join("owner-recovery.log"),
        "CleanupUnconfirmed: original guardian cleanup failed\n",
    )
    .unwrap();
    let config = file_server::Config {
        userapp_workspace_dir: root.path().join("no-source"),
        log_base_dir: root.path().join("logs"),
        app_cli_admin_probe_addr: "127.0.0.1:1".into(),
        ..Default::default()
    };
    let fs = file_server::FileServer::builder(config).build().unwrap();
    let router = crate::routes::userapp_top_router()
        .split_for_parts()
        .0
        .with_state(UserAppState::new(fs.state()));
    let selection =
        json!({"selectors":[{"service_id":"app-cli", "source_ids":["owner-recovery"]}]});
    let sources = body(post(&router, "sources/query", selection.clone()).await).await;
    assert_eq!(sources["success"], true);
    assert!(
        sources["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|source| source["source_id"] == "owner-recovery"
                && source["matched_files"] == json!(["owner-recovery.log"]))
    );
    let snapshot = body(post(&router, "query", selection.clone()).await).await;
    assert_eq!(snapshot["success"], true);
    assert!(
        snapshot["data"]["logs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|log| log["message"]
                .as_str()
                .unwrap()
                .contains("original guardian cleanup failed"))
    );
    let mut stream = post(&router, "stream", selection)
        .await
        .into_body()
        .into_data_stream();
    let first = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8(first.to_vec())
            .unwrap()
            .contains("CleanupUnconfirmed")
    );
    drop(stream);
    assert!(!root.path().join("no-source").exists());
}

#[tokio::test]
async fn running_stream_observes_published_catalog_replacement() {
    let root = tempfile::tempdir().unwrap();
    let (router, state_root) = fixture(root.path(), "stopped");
    let response = post(
        &router,
        "stream",
        json!({"selectors":[{"service_id":"api", "source_ids":["application"]}]}),
    )
    .await;
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(
        String::from_utf8(first.to_vec())
            .unwrap()
            .contains("event: log")
    );
    let checkpoint = stream.next().await.unwrap().unwrap();
    assert!(
        String::from_utf8(checkpoint.to_vec())
            .unwrap()
            .contains("event: checkpoint")
    );
    let mut next = release();
    next.release_id = "stream-next-release".into();
    LogCatalog::from_release(
        "227".into(),
        root.path().join("workspaces/227"),
        root.path().join("platform-logs/227/app-cli"),
        next,
        LogLayout::Builtin,
    )
    .unwrap()
    .publish(&state_root)
    .unwrap();
    let replacement = tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8(replacement.to_vec())
            .unwrap()
            .contains("event: cursor_reset")
    );
    drop(stream);
}
