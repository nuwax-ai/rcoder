use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::response::IntoResponse as _;
use axum::{Router, routing::post};
use shared_types::UserappDevLocator;
use tower::ServiceExt as _;

use crate::test_support::{MockRuntime, test_service};

struct ReadOnlyLocator {
    address: Option<String>,
    ensure_calls: AtomicUsize,
    read_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl UserappDevLocator for ReadOnlyLocator {
    async fn dev_file_server_addr(&self, _: &str) -> Result<String, String> {
        self.ensure_calls.fetch_add(1, Ordering::SeqCst);
        Err("log reads must not enter builder ensure".into())
    }

    async fn dev_logs_file_server_addr(&self, _: &str) -> Result<String, String> {
        self.read_calls.fetch_add(1, Ordering::SeqCst);
        self.address
            .clone()
            .ok_or_else(|| "Development container is not running; file logs are unavailable".into())
    }

    async fn dev_container_alive(&self, _: &str) -> Result<bool, String> {
        Ok(self.address.is_some())
    }
}

fn public_router(state: Arc<AppManagerState>) -> Router {
    Router::new()
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/logs/sources/query",
            post(query_app_log_sources),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/logs/query",
            post(query_app_logs),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/logs/stream",
            post(stream_app_logs_v1),
        )
        .with_state(state)
}

#[tokio::test]
async fn stopped_dev_logs_forward_without_owner_or_builder_ensure() {
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let upstream_calls = calls.clone();
    let upstream = Router::new().fallback(move |uri: axum::http::Uri| {
        let calls = upstream_calls.clone();
        async move {
            calls.lock().unwrap().push(uri.path().to_owned());
            match uri.path() {
                "/api/v1/userapp/app1/dev/logs/sources/query" => Json(serde_json::json!({
                    "success":true,"code":"OK","message":"ok",
                    "data":[{"service_id":"app-cli","source_id":"orchestrator","format":"json","matched_files":["/app/logs/app-cli.log"]}]
                })).into_response(),
                "/api/v1/userapp/app1/dev/logs/query" => Json(serde_json::json!({
                    "success":true,"code":"OK","message":"ok",
                    "data":{"logs":[{"line":"startup failed: project origin mismatch"}],"cursor":"stopped-cursor","source_errors":[],"cursor_reset":false}
                })).into_response(),
                "/api/v1/userapp/app1/dev/logs/stream" => (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    "event: log\ndata: {\"line\":\"startup failed\"}\n\n",
                ).into_response(),
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let locator = Arc::new(ReadOnlyLocator {
        address: Some(address),
        ensure_calls: AtomicUsize::new(0),
        read_calls: AtomicUsize::new(0),
    });
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root.path(), runtime.clone()).await;
    service.set_dev_locator(locator.clone()).unwrap();
    let router = public_router(Arc::new(AppManagerState {
        app_service: Arc::new(service),
        http_client: reqwest::Client::builder().no_proxy().build().unwrap(),
    }));
    for suffix in ["sources/query", "query", "stream"] {
        let request = axum::http::Request::post(format!(
            "/api/v1/userapp/app1/dev/logs/{suffix}?user_id=user1"
        ))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{suffix}");
        let body = axum::body::to_bytes(response.into_body(), 16_384)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains(if suffix == "sources/query" {
            "orchestrator"
        } else {
            "startup failed"
        }));
    }
    assert_eq!(locator.ensure_calls.load(Ordering::SeqCst), 0);
    assert_eq!(locator.read_calls.load(Ordering::SeqCst), 3);
    assert_eq!(calls.lock().unwrap().len(), 3);
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.management_start_calls.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn missing_dev_log_container_reports_unavailable_without_waking_it() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root.path(), runtime.clone()).await;
    let locator = Arc::new(ReadOnlyLocator {
        address: None,
        ensure_calls: AtomicUsize::new(0),
        read_calls: AtomicUsize::new(0),
    });
    service.set_dev_locator(locator.clone()).unwrap();
    let error = service
        .log_api_base(shared_types::UserappStage::Dev, "app1")
        .await
        .unwrap_err();
    assert!(error.message().contains("container is not running"));
    assert_eq!(locator.ensure_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn prod_log_routes_keep_the_existing_management_protocol() {
    assert_eq!(
        log_path(
            shared_types::UserappStage::Prod,
            "app1",
            LogOperation::Sources
        ),
        "/v1/logs/sources/query"
    );
    assert_eq!(
        log_path(
            shared_types::UserappStage::Prod,
            "app1",
            LogOperation::Query
        ),
        "/v1/logs/query"
    );
    assert_eq!(
        log_path(
            shared_types::UserappStage::Prod,
            "app1",
            LogOperation::Stream
        ),
        "/v1/logs/stream"
    );
}
