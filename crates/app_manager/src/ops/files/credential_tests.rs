use super::*;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct ProtectedDev(String);
#[async_trait::async_trait]
impl shared_types::UserappDevLocator for ProtectedDev {
    async fn dev_file_server_addr(&self, _: &str) -> Result<String, String> {
        Ok(self.0.clone())
    }
    async fn dev_container_alive(&self, _: &str) -> Result<bool, String> {
        Ok(true)
    }
}
#[derive(Clone)]
struct FixedCredentials(
    Result<shared_types::FileServerRequestCredentials, shared_types::WakeFailure>,
);
#[async_trait::async_trait]
impl shared_types::FileServerCredentialsProvider for FixedCredentials {
    async fn for_target(
        &self,
        stage: UserappStage,
        app_id: &str,
        _: tokio::time::Instant,
    ) -> Result<shared_types::FileServerRequestCredentials, shared_types::WakeFailure> {
        assert_eq!(stage, UserappStage::Dev);
        assert_eq!(app_id, "fixtureapp");
        self.0.clone()
    }
}

#[tokio::test]
async fn file_operations_reach_optional_token_peer_and_never_send_control_key() {
    for (required, configured, success) in [
        (false, None, true),
        (true, Some("fixture-file-token"), true),
        (true, None, false),
        (true, Some("wrong-token"), false),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let serve_calls = calls.clone();
        let handler = move |headers: HeaderMap, uri: axum::http::Uri| {
            let calls = serve_calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                assert!(!headers.contains_key("x-api-key"));
                let valid = !required
                    || headers.get("x-proxy-token").and_then(|v| v.to_str().ok())
                        == Some("fixture-file-token");
                if !valid {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(serde_json::json!({"message":"missing configured file token"})),
                    );
                }
                let body = if uri.path().ends_with("/list") {
                    serde_json::json!({"success":true,"files":[{"path":"code/main.rs","size":1,"is_dir":false,"modified_at":"2026-10-06T00:00:00Z"}]})
                } else if uri.path().ends_with("/delete") {
                    serde_json::json!({"success":true})
                } else {
                    serde_json::json!({"success":true,"file_path":"code/main.rs","file_size":1,"uploaded_at":"2026-10-06T00:00:00Z"})
                };
                (StatusCode::OK, Json(body))
            }
        };
        let router = Router::new()
            .route("/api/v1/userapp/app-files/list", get(handler.clone()))
            .route("/api/v1/userapp/app-files/upload", post(handler.clone()))
            .route(
                "/api/v1/userapp/app-files/upload-from-url",
                post(handler.clone()),
            )
            .route("/api/v1/userapp/app-files/delete", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let base = format!("http://{}", listener.local_addr().expect("fixture address"));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("fixture server");
        });
        let root = tempfile::tempdir().expect("test root");
        let service = crate::test_support::test_service(root.path(), Arc::default()).await;
        service
            .set_dev_locator(Arc::new(ProtectedDev(base)))
            .expect("locator");
        service
            .set_file_credentials_provider(Arc::new(FixedCredentials(Ok(
                shared_types::FileServerRequestCredentials {
                    proxy_token: configured.map(str::to_owned),
                },
            ))))
            .expect("credentials");
        let list = service
            .list_files(UserappStage::Dev, "fixtureapp", None)
            .await;
        assert_eq!(list.is_ok(), success, "list: {list:?}");
        if success {
            assert_eq!(list.unwrap()[0].path, "code/main.rs");
        }
        let upload = service
            .upload_file(
                UserappStage::Dev,
                "fixtureapp",
                vec![1],
                "code/main.rs",
                false,
            )
            .await;
        assert_eq!(upload.is_ok(), success, "upload: {upload:?}");
        let from_url = service
            .upload_from_url(
                UserappStage::Dev,
                "fixtureapp",
                "https://fixture.invalid/main.rs",
                "code/main.rs",
                false,
            )
            .await;
        assert_eq!(from_url.is_ok(), success, "from-url: {from_url:?}");
        let delete = service
            .delete_file(UserappStage::Dev, "fixtureapp", "code/main.rs")
            .await;
        assert_eq!(delete.is_ok(), success, "delete: {delete:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        server.abort();
        drop(server.await);
    }
}

#[tokio::test]
async fn credential_query_failure_keeps_cause_and_never_dispatches_file_request() {
    let calls = Arc::new(AtomicUsize::new(0));
    let serve_calls = calls.clone();
    let router = Router::new().route(
        "/api/v1/userapp/app-files/list",
        get(move || {
            let calls = serve_calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("fixture listener");
    let base = format!("http://{}", listener.local_addr().expect("fixture address"));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("fixture server");
    });
    let root = tempfile::tempdir().expect("test root");
    let service = crate::test_support::test_service(root.path(), Arc::default()).await;
    service
        .set_dev_locator(Arc::new(ProtectedDev(base)))
        .expect("locator");
    service
        .set_file_credentials_provider(Arc::new(FixedCredentials(Err(
            shared_types::WakeFailure::new(
                shared_types::ERR_RUNTIME_TIMEOUT,
                "file_credentials_configuration",
                "Container spec query timed out",
            ),
        ))))
        .expect("credentials");
    let error = service
        .list_files(UserappStage::Dev, "fixtureapp", None)
        .await
        .expect_err("query failed");
    assert_eq!(error.code(), shared_types::ERR_RUNTIME_TIMEOUT);
    assert_ne!(error.code(), shared_types::ERR_APP_NOT_FOUND);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    server.abort();
    drop(server.await);
}
