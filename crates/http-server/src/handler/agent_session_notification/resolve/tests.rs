//! Real PostgreSQL closure is not an authoritative missing-session observation.
use super::*;
use crate::handler::userapp_agent_target::test_support::Fixture;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
    routing::get,
};
use rcoder_storage::{config::PostgresConfig, pg::PgStore};
use std::time::Duration;
use tower::ServiceExt as _;

async fn closed_database_state(fixture: &Fixture) -> Option<Arc<crate::app_state::AppState>> {
    let dsn = std::env::var("RCODER_PG_TEST_DSN")
        .ok()
        .filter(|dsn| !dsn.is_empty());
    assert!(
        dsn.is_some() || std::env::var("RCODER_PG_TEST_STRICT").as_deref() != Ok("1"),
        "Strict PG tests require a disposable DSN"
    );
    let dsn = dsn?;
    let store = PgStore::connect(
        &PostgresConfig {
            url: Some(dsn),
            ..Default::default()
        },
        "isolated-sse-test".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap();
    assert!(
        store
            .shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
    let mut state = (*fixture.state).clone();
    state.projects = Arc::new(crate::storage::ProjectStoreBackend::Postgres(Arc::new(
        store,
    )));
    Some(Arc::new(state))
}

#[tokio::test]
async fn ordinary_session_read_error_is_retryable_503_instead_of_not_found() {
    let fixture = Fixture::new().await;
    let Some(state) = closed_database_state(&fixture).await else {
        return;
    };
    let sid = "ordinary-session-after-db-close";
    let error = lookup_project_info_by_session(&state, sid)
        .await
        .err()
        .expect("closed DB cannot be a miss");
    assert_eq!(error.status_code(), StatusCode::SERVICE_UNAVAILABLE);
    let body = serde_json::to_value(error.into_http_result::<String>("zh-CN")).unwrap();
    assert_eq!(body["code"], shared_types::ERR_SERVICE_UNAVAILABLE);
    assert_eq!(body["error_detail"]["task_id"], sid);
    assert_eq!(body["error_detail"]["retryable"], true);
    assert_eq!(body["error_detail"]["stage"], "sse.session_lookup");
}

#[tokio::test]
async fn full_sse_entry_preserves_prior_userapp_lookup_failure_instead_of_404() {
    let fixture = Fixture::new().await;
    let Some(state) = closed_database_state(&fixture).await else {
        return;
    };
    // Domain selection intentionally remains UserApp-first. Its binding lives
    // in the same project PG store, so this earlier error remains honest 500.
    let app = Router::new()
        .route(
            "/computer/progress/{session_id}",
            get(crate::handler::agent_session_notification::computer_agent_progress_notification),
        )
        .with_state(state);
    let response = app
        .oneshot(
            Request::get("/computer/progress/whole-store-closed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    assert_ne!(body["code"], shared_types::ERR_SESSION_NOT_FOUND);
    assert_eq!(body["success"], false);
}
