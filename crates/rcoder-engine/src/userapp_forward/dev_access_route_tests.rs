//! Real public route table plus the real lifecycle store. These assertions
//! cover admission and read-only routing; they are not container acceptance.
use axum::{
    body::{Body, to_bytes},
    http::{Method, Request},
};
use shared_types::{ERR_CONTAINER_NOT_FOUND, ERR_OPERATION_IN_PROGRESS, UserAppOperationScope};
use std::time::Duration;
use tower::ServiceExt;

async fn request(router: axum::Router, method: Method, path: &str) -> serde_json::Value {
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        router.oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from("{\"app_id\":\"accessapp\"}"))
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn real_task_logs_status_and_stop_routes_do_not_wake_completed_physical_stop() {
    let (state, app, _directory) = crate::userapp_builder::builder_access_test_state().await;
    let original = crate::userapp_builder::builder_stop_fixture(&state, &app, true).await;
    let before = state
        .userapp_store
        .get_compute_control(&app.app_id, &original.operation_id)
        .await
        .unwrap()
        .unwrap();
    let router = super::routes().with_state(state.clone());
    for (method, path, code) in [
        (
            Method::GET,
            "/api/v1/userapp/tasks/original-task?app_id=accessapp",
            ERR_CONTAINER_NOT_FOUND,
        ),
        (
            Method::GET,
            "/api/v1/userapp/tasks/original-task/logs/stream?app_id=accessapp",
            ERR_CONTAINER_NOT_FOUND,
        ),
        (
            Method::GET,
            "/api/v1/userapp/dev/list?app_id=accessapp",
            ERR_CONTAINER_NOT_FOUND,
        ),
        (
            Method::GET,
            "/api/v1/userapp/dev/framework-info?app_id=accessapp",
            ERR_CONTAINER_NOT_FOUND,
        ),
        (
            Method::POST,
            "/api/v1/userapp/dev/stop",
            shared_types::SUCCESS,
        ),
        (
            Method::POST,
            "/api/v1/userapp/tasks/original-task/cancel?app_id=accessapp",
            shared_types::SUCCESS,
        ),
    ] {
        let body = request(router.clone(), method, path).await;
        assert_eq!(body["code"], code, "{path}: {body}");
        let current = state
            .userapp_store
            .read_compute_status(&app.app_id, &app.lifecycle_id, UserAppOperationScope::Dev)
            .await
            .unwrap();
        assert!(current.desired_stopped, "{path} must not wake compute");
        assert_eq!(current.operation.unwrap(), before);
        assert!(
            state
                .userapp_store
                .get_application(&app.app_id)
                .await
                .unwrap()
                .unwrap()
                .active_operations
                .dev
                .is_none()
        );
    }
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn real_source_routes_keep_original_stop_blocker_until_physical_execution_settles() {
    for path in [
        "/api/v1/userapp/build",
        "/api/v1/userapp/dev/start",
        "/api/v1/userapp/dev/restart",
    ] {
        let (mut state, app, _directory) =
            crate::userapp_builder::builder_access_test_state().await;
        let original = crate::userapp_builder::builder_stop_fixture(&state, &app, false).await;
        let config = &mut std::sync::Arc::get_mut(&mut state)
            .unwrap()
            .config
            .userapp_storage;
        config.ensure_timeout_seconds = 1;
        config.interactive_ensure_wait_seconds = 1;
        let router = super::routes().with_state(state.clone());
        let body = request(router, Method::POST, path).await;
        assert_eq!(body["code"], ERR_OPERATION_IN_PROGRESS, "{path}: {body}");
        assert_eq!(body["blocker"]["operation_id"], original.operation_id);
        assert!(
            state
                .userapp_store
                .compute_desired_stopped(&app.app_id, &app.lifecycle_id, UserAppOperationScope::Dev)
                .await
                .unwrap()
        );
        assert_eq!(
            state
                .userapp_store
                .get_compute_control(&app.app_id, &original.operation_id)
                .await
                .unwrap()
                .unwrap(),
            original
        );
        assert!(
            state
                .userapp_store
                .get_application(&app.app_id)
                .await
                .unwrap()
                .unwrap()
                .active_operations
                .dev
                .is_none(),
            "the blocked source request must not admit another builder or business executor"
        );
        state.userapp_store_control.shutdown().await.unwrap();
    }
}
