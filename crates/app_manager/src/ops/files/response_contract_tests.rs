use axum::body::Body;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt as _;
use std::sync::Arc;
use tracing::instrument::WithSubscriber as _;

struct FileResponseLocator(String);
#[async_trait::async_trait]
impl shared_types::UserappDevLocator for FileResponseLocator {
    async fn dev_file_server_addr(&self, _: &str) -> Result<String, String> {
        Ok(self.0.clone())
    }
    async fn dev_container_alive(&self, _: &str) -> Result<bool, String> {
        Ok(true)
    }
}
async fn file_service(root: &std::path::Path, base: String) -> crate::service::AppService {
    let service = crate::test_support::test_service(root, Arc::default()).await;
    service
        .set_dev_locator(Arc::new(FileResponseLocator(base)))
        .unwrap();
    service
}
async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (base, server)
}
async fn envelope(error: crate::AppOperationError) -> serde_json::Value {
    let response = shared_types::AppError::from(error).into_response();
    let body = axum::body::to_bytes(response.into_body(), 16_384)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn bare_file_peer_404_is_not_application_absence() {
    let (base, server) = serve(Router::new().route(
        "/api/v1/userapp/app-files/list",
        get(|| async { (StatusCode::NOT_FOUND, "route not registered") }),
    ))
    .await;
    let root = tempfile::tempdir().unwrap();
    let service = file_service(root.path(), base).await;
    let response = envelope(
        service
            .list_files(shared_types::UserappStage::Dev, "fixtureapp", None)
            .await
            .unwrap_err(),
    )
    .await;
    assert_ne!(
        response["code"], "ERR_APP_NOT_FOUND",
        "peer404 is not authoritative application absence"
    );
    server.abort();
}

#[tokio::test]
async fn file_peer_structured_error_keeps_original_code_identity_and_detail() {
    let blocker = serde_json::json!({"scope":"Prod","operation_id":"original-blocking-file-stop","kind":"Stop","state":"Running","step":"stop"});
    let expected_blocker = blocker.clone();
    let (base,server) = serve(Router::new().route("/api/v1/userapp/app-files/list",
        get(move ||{let blocker=blocker.clone();async move {
            (StatusCode::SERVICE_UNAVAILABLE,Json(serde_json::json!({
                "success":false,"code":"ERR_DATABASE_NOT_READY","message":"Database is not ready",
                "operation_id":"original-file-operation","blocker":blocker,
                "error_detail":{"reason_code":"ERR_DATABASE_NOT_READY","stage":"original_file_peer",
                    "detail":"Database is not ready","hint":"Inspect original task","retryable":true,
                    "task_id":"original-file-task","service_id":"postgres"}
            })))
        }}))).await;
    let root = tempfile::tempdir().unwrap();
    let service = file_service(root.path(), base).await;
    let response = envelope(
        service
            .list_files(shared_types::UserappStage::Dev, "fixtureapp", None)
            .await
            .unwrap_err(),
    )
    .await;
    assert_eq!(response["code"], "ERR_DATABASE_NOT_READY");
    assert_eq!(response["operation_id"], "original-file-operation");
    assert_eq!(response["blocker"], expected_blocker);
    assert_eq!(response["error_detail"]["stage"], "original_file_peer");
    assert_eq!(response["error_detail"]["task_id"], "original-file-task");
    server.abort();
}

// Defensive protocol test: current app-files handlers emit non-2xx errors.
#[tokio::test]
async fn file_delete_http_200_false_is_never_reported_as_success() {
    let (base,server) = serve(Router::new().route("/api/v1/userapp/app-files/delete",
        post(||async {Json(serde_json::json!({"success":false,"code":"ERR_VALIDATION","message":"delete target rejected"}))}))).await;
    let root = tempfile::tempdir().unwrap();
    let service = file_service(root.path(), base).await;
    let error = service
        .delete_file(
            shared_types::UserappStage::Dev,
            "fixtureapp",
            "code/main.rs",
        )
        .await
        .expect_err("the peer rejected the delete");
    let response = envelope(error).await;
    assert_ne!(response["code"], "0000");
    assert_eq!(response["success"], false);
    server.abort();
}

#[tokio::test]
async fn file_error_body_timeout_retains_runtime_timeout() {
    let (base, server) = serve(Router::new().route(
        "/error",
        get(|| async {
            let first = futures_util::stream::once(async { Ok::<_, std::io::Error>("{") });
            let pending = futures_util::stream::pending::<Result<&'static str, std::io::Error>>();
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header("content-type", HeaderValue::from_static("application/json"))
                .body(Body::from_stream(first.chain(pending)))
                .unwrap()
        }),
    ))
    .await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{base}/error"))
        .timeout(std::time::Duration::from_millis(100))
        .send()
        .await
        .unwrap();
    let error = super::check_status(response, "list-files", "fixtureapp")
        .await
        .unwrap_err();
    let response = envelope(error).await;
    assert_eq!(response["code"], "ERR_RUNTIME_TIMEOUT");
    server.abort();
}

#[derive(Clone)]
struct FileLogSink(Arc<std::sync::Mutex<Vec<String>>>);
struct FileLogVisitor<'a>(&'a mut String);
impl tracing::field::Visit for FileLogVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={value:?};", field.name()));
    }
}
impl tracing::Subscriber for FileLogSink {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut FileLogVisitor(&mut line));
        self.0.lock().unwrap().push(line);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
#[tokio::test]
async fn file_error_body_is_redacted_in_response_and_warning() {
    let (base,server)=serve(Router::new().route("/error",get(||async {
        (StatusCode::INTERNAL_SERVER_ERROR,Json(serde_json::json!({"success":false,"code":"UNKNOWN_ERROR",
            "error":{"type":"SYSTEM_ERROR","message":"password=fixture-file-password token=fixture-file-token","requestId":"source-request"}})))
    }))).await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{base}/error"))
        .send()
        .await
        .unwrap();
    let sink = FileLogSink(Arc::default());
    let error = super::check_status(response, "list-files", "fixtureapp")
        .with_subscriber(sink.clone())
        .await
        .unwrap_err();
    let response = envelope(error).await.to_string();
    let warning = sink.0.lock().unwrap().join("\n");
    assert!(!warning.is_empty(), "exercise actual warning event");
    for secret in ["fixture-file-password", "fixture-file-token"] {
        assert!(
            !response.contains(secret) && !warning.contains(secret),
            "credential appeared in response or warning; response={response}, warning={warning}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn structured_file_resource_and_application_absence_remain_distinct() {
    for (body, expected) in [
        (
            serde_json::json!({"success":false,"code":"UNKNOWN_ERROR","error":{"type":"RESOURCE_ERROR","message":"file does not exist"}}),
            "ERR_FILE_NOT_FOUND",
        ),
        (
            serde_json::json!({"success":false,"code":"ERR_APP_NOT_FOUND","message":"Authoritative target application does not exist"}),
            "ERR_APP_NOT_FOUND",
        ),
    ] {
        let (base, server) = serve(Router::new().route(
            "/api/v1/userapp/app-files/list",
            get(move || {
                let body = body.clone();
                async move { (StatusCode::NOT_FOUND, Json(body)) }
            }),
        ))
        .await;
        let root = tempfile::tempdir().unwrap();
        let service = file_service(root.path(), base).await;
        let error = service
            .list_files(shared_types::UserappStage::Dev, "fixtureapp", None)
            .await
            .unwrap_err();
        let response = envelope(error).await;
        assert_eq!(response["code"], expected);
        server.abort();
    }
}

#[tokio::test]
async fn dispatched_file_write_body_timeout_keeps_unknown_result_and_cause() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let peer_calls = calls.clone();
    let (base, server) = serve(Router::new().route(
        "/write-error",
        post(move || {
            let calls = peer_calls.clone();
            async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let first = futures_util::stream::once(async { Ok::<_, std::io::Error>("{") });
                let pending =
                    futures_util::stream::pending::<Result<&'static str, std::io::Error>>();
                Response::builder()
                    .status(StatusCode::SERVICE_UNAVAILABLE)
                    .body(Body::from_stream(first.chain(pending)))
                    .unwrap()
            }
        }),
    ))
    .await;
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{base}/write-error"))
        .timeout(std::time::Duration::from_millis(100))
        .send()
        .await
        .unwrap();
    let error = super::check_status_with_context(
        response,
        "delete-file",
        "fixtureapp",
        super::FileForwardContext::Mutation,
    )
    .await
    .unwrap_err();
    assert!(error.requires_recovery());
    let response = envelope(error).await;
    assert_eq!(response["code"], "ERR_OPERATION_OUTCOME_UNKNOWN");
    assert_eq!(
        response["error_detail"]["reason_code"],
        "ERR_RUNTIME_TIMEOUT"
    );
    assert_eq!(response["error_detail"]["retryable"], false);
    assert!(
        response.get("operation_id").is_none(),
        "no operation id is invented for an untracked file request"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "never replays the write"
    );
    server.abort();
}
