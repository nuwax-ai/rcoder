//! Exercise the production assembly, including preview's independently guarded fallback.
use super::apply_route_security;
use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::{get, post},
};
use shared_types::{
    ExecutorLogChunk, ExecutorStartTicket, ExecutorStopOutcome, ExecutorVerifyReport,
    PreviewExecutor, PreviewExecutorError,
};
use tower::ServiceExt;

const SERVICE_KEY: &str = "assembly-service-key";
const PEER_TOKEN: &str = "assembly-peer-token";
const PROGRESS: &str = "/computer/progress/{session_id}";
const VERIFY: &str = "/api/v1/preview-internal/verify";

struct NoPreviewExecution;

#[async_trait::async_trait]
impl PreviewExecutor for NoPreviewExecution {
    async fn start_local(
        &self,
        _ticket: &ExecutorStartTicket,
    ) -> Result<(i64, u16), PreviewExecutorError> {
        unreachable!("route assembly must not start a preview")
    }

    async fn stop_local(
        &self,
        _preview_key: &str,
        _instance_id: &str,
    ) -> Result<ExecutorStopOutcome, PreviewExecutorError> {
        unreachable!("route assembly must not stop a preview")
    }

    async fn verify_local(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<ExecutorVerifyReport, PreviewExecutorError> {
        assert_eq!((preview_key, instance_id), ("absent", "absent"));
        Ok(ExecutorVerifyReport {
            identity_match: false,
            alive: false,
            pid: None,
            port: None,
        })
    }

    async fn registration_matches(
        &self,
        _preview_key: &str,
        _instance_id: &str,
    ) -> Result<bool, PreviewExecutorError> {
        unreachable!("route assembly must not route a preview")
    }

    async fn read_log_local(
        &self,
        _log_key: &str,
        _log_type: &str,
        _start_index: usize,
    ) -> Result<ExecutorLogChunk, PreviewExecutorError> {
        unreachable!("route assembly must not read preview logs")
    }
}

fn assembled(enabled: bool) -> Router {
    let coordinator = preview_coordinator::PreviewCoordinator::new(
        Arc::new(preview_coordinator::InProcessPreviewStore::new()),
        Arc::new(NoPreviewExecution),
        Arc::new(preview_coordinator::SingleInstanceEvidence),
        PEER_TOKEN.into(),
        preview_coordinator::CoordinatorConfig::default(),
    );
    apply_route_security(
        Router::new()
            .route(PROGRESS, get(|| async { "progress" }))
            .route("/chat", post(|| async { "accepted" })),
        Router::new().route("/internal/pod/ensure", post(|| async { "accepted" })),
        // Use the real preview router and token guard. Its Router::layer also
        // wraps the default fallback, which can replace the prior fallback on merge.
        preview_coordinator::internal_router(Arc::new(coordinator)),
        Router::new().route("/api/computer/fs/roots", get(|| async { "accepted" })),
        Arc::new(arc_swap::ArcSwap::from_pointee(
            shared_types::ApiKeyAuthConfig {
                enabled,
                api_key: SERVICE_KEY.into(),
            },
        )),
    )
}

async fn request(
    app: &Router,
    path: &str,
    method: &str,
    headers: &[(&str, &str)],
    expected: StatusCode,
) -> Vec<u8> {
    let mut builder = Request::builder().uri(path).method(method);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let body = if path == VERIFY {
        builder = builder.header("content-type", "application/json");
        Body::from(r#"{"previewKey":"absent","instanceId":"absent"}"#)
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), expected, "{method} {path}");
    for (name, value) in [
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "strict-origin-when-cross-origin"),
        ("x-xss-protection", "0"),
    ] {
        assert_eq!(
            response.headers().get(name).unwrap(),
            value,
            "{path}: {name}"
        );
    }
    assert!(!response.headers().contains_key("x-frame-options"));
    to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

fn sample(output: &str, name: &str, path: &str, status: Option<u16>) -> u64 {
    let rows: Vec<_> = output
        .lines()
        .filter(|line| {
            line.starts_with(&format!("{name}{{"))
                && line.contains(&format!("path=\"{path}\""))
                && status.is_none_or(|status| line.contains(&format!("status=\"{status}\"")))
        })
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "missing or duplicate {name} series for {path}: {output}"
    );
    rows[0].rsplit_once(' ').unwrap().1.parse().unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn final_merge_records_templates_and_fallback_once_without_changing_auth_scope() {
    let telemetry = rcoder_telemetry::init_prometheus_only("assembly-test").unwrap();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let app = assembled(false);
    for path in [
        format!("/computer/progress/session-{nonce}-one?private={nonce}"),
        format!("/computer/progress/session-{nonce}-two?private={nonce}"),
    ] {
        request(&app, &path, "GET", &[], StatusCode::OK).await;
    }
    for path in [
        format!("/missing/{nonce}-one"),
        format!("/missing/{nonce}-two"),
    ] {
        request(&app, &path, "GET", &[], StatusCode::NOT_FOUND).await;
    }
    let metrics = telemetry.render_metrics().unwrap();
    assert_eq!(
        sample(&metrics, "http_requests_total", PROGRESS, Some(200)),
        2
    );
    assert_eq!(
        sample(
            &metrics,
            "http_request_duration_seconds_count",
            PROGRESS,
            None
        ),
        2
    );
    assert_eq!(
        sample(&metrics, "http_requests_total", "__unmatched__", Some(404)),
        2
    );
    assert_eq!(
        sample(
            &metrics,
            "http_request_duration_seconds_count",
            "__unmatched__",
            None
        ),
        2
    );
    assert!(
        !metrics.contains(&nonce),
        "raw parameter/query/unknown path leaked into labels"
    );

    for enabled in [false, true] {
        let app = assembled(enabled);
        for (path, method) in [
            ("/chat", "POST"),
            ("/internal/pod/ensure", "POST"),
            ("/api/computer/fs/roots", "GET"),
        ] {
            for key in [None, Some("incorrect"), Some(SERVICE_KEY)] {
                let headers: Vec<_> = key.map(|key| ("x-api-key", key)).into_iter().collect();
                let expected = if enabled && key != Some(SERVICE_KEY) {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::OK
                };
                let body = request(&app, path, method, &headers, expected).await;
                if expected == StatusCode::UNAUTHORIZED {
                    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(
                        error["code"],
                        shared_types::error_codes::ERR_API_KEY_AUTH_FAILED
                    );
                } else {
                    assert_eq!(body, b"accepted");
                }
            }
        }
        for headers in [
            vec![],
            vec![("x-api-key", SERVICE_KEY)],
            vec![
                ("x-api-key", SERVICE_KEY),
                (preview_coordinator::INTERNAL_TOKEN_HEADER, "incorrect"),
            ],
        ] {
            assert_eq!(
                request(&app, VERIFY, "POST", &headers, StatusCode::NOT_FOUND).await,
                b"Not Found"
            );
        }
        for key in [None, Some("incorrect"), Some(SERVICE_KEY)] {
            let mut headers = vec![(preview_coordinator::INTERNAL_TOKEN_HEADER, PEER_TOKEN)];
            headers.extend(key.map(|key| ("x-api-key", key)));
            let body = request(&app, VERIFY, "POST", &headers, StatusCode::OK).await;
            let verification: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                verification,
                serde_json::json!({"identityMatch": false, "alive": false, "pid": null, "port": null})
            );
        }
    }
    let metrics = telemetry.render_metrics().unwrap();
    assert_eq!(
        sample(&metrics, "http_requests_total", PROGRESS, Some(200)),
        2
    );
    assert_eq!(
        sample(
            &metrics,
            "http_request_duration_seconds_count",
            PROGRESS,
            None
        ),
        2
    );
    assert_eq!(
        sample(&metrics, "http_requests_total", "__unmatched__", Some(404)),
        2
    );
    assert_eq!(
        sample(
            &metrics,
            "http_request_duration_seconds_count",
            "__unmatched__",
            None
        ),
        2
    );
    assert_eq!(
        sample(&metrics, "http_requests_total", VERIFY, Some(200)),
        6
    );
    assert_eq!(
        sample(&metrics, "http_requests_total", VERIFY, Some(404)),
        6
    );
    assert_eq!(
        sample(
            &metrics,
            "http_request_duration_seconds_count",
            VERIFY,
            None
        ),
        12
    );
    assert!(
        !metrics.contains(&nonce),
        "raw URI escaped after guarded requests"
    );
}
