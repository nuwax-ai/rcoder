use super::StartAppRequest;
use crate::test_support::{MockRuntime, test_service};
use std::sync::Arc;

#[test]
fn hot_capability_accepts_operation_identity_and_quiescent_protocols() {
    for (version, supported) in [(0, false), (1, false), (2, false), (3, false), (4, true)] {
        assert_eq!(
            super::supports_hot_protocol(&serde_json::json!({"data":{"protocol_version":version}})),
            supported
        );
    }
    assert!(!super::supports_hot_protocol(
        &serde_json::json!({"data":{}})
    ));
}

/// app 不存在 → 回退换 Pod（None），不触发任何容器调用。
#[tokio::test]
async fn hot_deploy_falls_back_when_app_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    let svc = test_service(tmp.path(), runtime.clone()).await;

    let outcome = svc
        .try_deploy_via_container_api(
            "appnope",
            "http://x/p.zip",
            "rel-1",
            "",
            &StartAppRequest {
                ..Default::default()
            },
        )
        .await
        .expect("fallback must not error");
    assert!(outcome.is_none(), "missing app must fall back to pod path");
}

/// app 在跑但未配 APP_CLI_DEPLOY_TOKEN → 回退（server 形态镜像未就位）。
#[tokio::test]
async fn hot_deploy_falls_back_without_token() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = Arc::new(MockRuntime::default());
    runtime.deployments.insert(
        "applive".into(),
        container_runtime_api::DeploymentStatus {
            app_id: "applive".into(),
            replicas: 1,
            ready_replicas: 1,
            phase: "Running".into(),
            ..Default::default()
        },
    );
    let svc = test_service(tmp.path(), runtime).await;

    let outcome = svc
        .try_deploy_via_container_api(
            "applive",
            "http://x/p.zip",
            "rel-1",
            "",
            &StartAppRequest {
                ..Default::default()
            },
        )
        .await
        .expect("fallback must not error");
    assert!(
        outcome.is_none(),
        "missing token must fall back to pod path"
    );
}

// /v1/deploy/status phase 解析（信封/裸顶层双兼容 + 未知相位容错）的
// 用例已随 `parse_deploy_status` 迁至 deploy_wait.rs（`parse_deploy_status_shapes`）。

/// deploy_mode wire：默认缺省（pod）+ hot 受理。
#[test]
fn deploy_mode_wire_default_and_hot() {
    let req: StartAppRequest =
        serde_json::from_str(r#"{"user_id":"u1","url":"http://x/p.zip"}"#).expect("parse");
    assert!(req.deploy_mode.is_none(), "default must be absent (= pod)");

    let req: StartAppRequest =
        serde_json::from_str(r#"{"user_id":"u1","url":"http://x/p.zip","deploy_mode":"hot"}"#)
            .expect("parse");
    assert_eq!(req.deploy_mode, Some(crate::models::DeployMode::Hot));

    // 非法值拒绝（枚举校验）
    assert!(
        serde_json::from_str::<StartAppRequest>(r#"{"url":"http://x/p.zip","deploy_mode":"fast"}"#)
            .is_err()
    );
}
#[tokio::test]
async fn confirmed_hot_failure_releases_ownership_while_pending_and_foreign_status_do_not() {
    for kubernetes in [false, true] {
        for (protocol, operation_id, phase, recovery, persisted, stage, release) in [
            (4, "current", "failed", None, true, "failed", true),
            (4, "current", "failed", None, false, "failed", false),
            (4, "current", "failed", None, true, "succeeded", true),
            (4, "current", "failed", None, false, "succeeded", false),
            (
                4,
                "current",
                "running",
                Some("restored"),
                true,
                "failed",
                true,
            ),
            (4, "current", "failed", Some("failed"), true, "failed", true),
            (
                4,
                "current",
                "orchestrating",
                Some("pending"),
                true,
                "failed",
                false,
            ),
            (
                4,
                "current",
                "failed",
                Some("unknown"),
                true,
                "failed",
                false,
            ),
            (4, "old", "failed", None, true, "failed", false),
            (2, "current", "failed", None, true, "failed", false),
        ] {
            let root = tempfile::tempdir().expect("directory");
            let runtime = Arc::new(MockRuntime::default());
            let mut service = test_service(root.path(), runtime.clone()).await;
            if kubernetes {
                service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
            }
            let guard = service
                .acquire_process_release_lock("hotterminal")
                .await
                .expect("lease");
            guard.mark_mutating().expect("accepted mutation");
            let document = serde_json::json!({"data": {"protocol_version":protocol,"phase":phase,"operation": {
                "operation_id":operation_id,"request_release_id":"release-b", "artifact_release_id":null,
                "deployment_generation_id":"generation", "persisted":persisted, "deploy_stage":stage,
                "phase":"failed","error":"download 404", "recovery": recovery.map(|status| serde_json::json!({
                    "status":status,"error":null,"database_migrations_reversed":false
                }))
            }}});
            let observed = super::observe_hot_operation(
                &document,
                "current",
                "release-b",
                "generation",
                &guard,
            );
            assert_eq!(observed.is_some(), release);
            let result = super::finish_hot_operation(
                guard,
                Err(crate::error::AppOperationError::Backend("failed".into())),
            )
            .await;
            assert!(result.is_err());
            let next = service
                .try_acquire_process_release_lock("hotterminal")
                .await;
            assert_eq!(
                next.is_ok(),
                release,
                "k8s={kubernetes}, phase={phase}, recovery={recovery:?}, persisted={persisted}, stage={stage}"
            );
            if let Ok(next) = next {
                next.finish().await.expect("release next");
            }
        }
    }
}

#[tokio::test]
async fn activated_hot_env_commit_failure_always_retains_recovery_ownership() {
    for failure in [1, 2] {
        let root = tempfile::tempdir().expect("directory");
        let runtime = Arc::new(MockRuntime::default());
        runtime
            .env_commit_failure
            .store(failure, std::sync::atomic::Ordering::SeqCst);
        let mut service = test_service(root.path(), runtime).await;
        service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
        let guard = service
            .acquire_process_release_lock("hot-env")
            .await
            .expect("lease");
        guard.mark_mutating().expect("accepted mutation");
        let result = super::converge_deploy_env_after_hot(
            service.runtime.as_ref(),
            service.config.access_mode,
            "hot-env",
            &Default::default(),
            &Default::default(),
            &guard,
        )
        .await;
        assert!(result.is_err());
        assert!(
            super::finish_hot_operation(guard, result.map(|()| Some(())))
                .await
                .is_err()
        );
        let next = service.try_acquire_process_release_lock("hot-env").await;
        assert!(
            next.is_err(),
            "activation must remain recoverable after env failure {failure}"
        );
        if let Ok(next) = next {
            next.finish().await.expect("release");
        }
    }
}
#[test]
fn hot_env_snapshot_is_rechecked_before_fallback_or_admission() {
    use std::collections::HashMap;
    let request = HashMap::from([("BUSINESS".into(), "old".into())]);
    let mut current = request.clone();
    current.insert("APP_DEPLOY_OPERATION_ID".into(), "platform".into());
    assert!(super::validate_requested_hot_env(&request, &current).is_ok());
    current.insert("BUSINESS".into(), "newer-concurrent-value".into());
    assert!(super::validate_requested_hot_env(&request, &current).is_err());
}

#[tokio::test]
async fn running_requires_exact_generation_successful_stage_and_persisted_receipt() {
    let root = tempfile::tempdir().expect("directory");
    let service = test_service(root.path(), Arc::new(MockRuntime::default())).await;
    let guard = service
        .acquire_process_release_lock("hotidentity")
        .await
        .expect("lease");
    for (generation, stage, persisted, accepted) in [
        ("generation-b", "succeeded", true, true),
        ("generation-a", "succeeded", true, false),
        ("generation-b", "pending", true, false),
        ("generation-b", "failed", true, false),
        ("generation-b", "succeeded", false, false),
    ] {
        let document = serde_json::json!({"data":{
            "protocol_version":4,"phase":"running","operation":{
                "operation_id":"operation-b","request_release_id":"request-b",
                "artifact_release_id":"artifact-from-manifest",
                "deployment_generation_id":generation,"deploy_stage":stage,
                "persisted":persisted,"phase":"running","error":null
            }
        }});
        assert_eq!(
            super::observe_hot_operation(
                &document,
                "operation-b",
                "request-b",
                "generation-b",
                &guard
            )
            .is_some(),
            accepted
        );
    }
    guard.finish().await.expect("finish");
}

#[tokio::test]
async fn accepted_hot_coordinator_survives_cancelled_caller() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    for timeout in [false, true] {
        let root = tempfile::tempdir().expect("directory");
        let runtime = Arc::new(MockRuntime::default());
        // If the Docker path accidentally calls mutable-env convergence, it fails.
        runtime
            .env_commit_failure
            .store(2, std::sync::atomic::Ordering::SeqCst);
        let service = test_service(root.path(), runtime.clone()).await;
        let guard = service
            .acquire_process_release_lock("hotcancel")
            .await
            .expect("lease");
        let accepted = Arc::new(tokio::sync::Notify::new());
        let post_accepted = accepted.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let router = Router::new()
        .route("/v1/deploy", post(move |Json(body): Json<serde_json::Value>| {
            let post_accepted = post_accepted.clone();
            async move {
            assert_eq!(body["deployment_generation_id"], "generation");
            post_accepted.notify_one();
            axum::http::StatusCode::ACCEPTED
            }
        }))
        .route("/ready", get(|| async { Json(serde_json::json!({"status":"ready","phase":"running"})) }))
        .route("/v1/deploy/status", get(|| async {
            Json(serde_json::json!({"data":{
                "protocol_version":4,"phase":"running","operation":{
                    "operation_id":"operation","request_release_id":"request",
                    "deployment_generation_id":"generation","artifact_release_id":"artifact",
                    "deploy_stage":"succeeded","persisted":true,"phase":"running","error":null
                }
            }}))
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        let task = super::HotDeploymentTask {
            runtime,
            access_mode: crate::config::AppAccessMode::Docker,
            app_id: "hotcancel".into(),
            url: "http://artifact".into(),
            release_id: "request".into(),
            sha256: String::new(),
            base: format!("http://{address}"),
            token: "token".into(),
            env_snapshot: Default::default(),
            generation_id: "generation".into(),
            operation_id: "operation".into(),
            operation: Arc::new(guard),
            budget: std::time::Duration::from_secs(300),
        };
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(async move {
            let worker = tokio::spawn(async move {
                let result = task.run().await;
                drop(
                    done_tx.send(
                        result
                            .as_ref()
                            .map(|value| *value)
                            .map_err(ToString::to_string),
                    ),
                );
                result
            });
            super::await_hot_coordinator(worker, std::time::Duration::from_millis(20)).await
        });
        tokio::select! {
            outcome = &mut done_rx => panic!("coordinator finished before admission: {outcome:?}"),
            admitted = tokio::time::timeout(std::time::Duration::from_secs(10), accepted.notified()) => {
                admitted.expect("admission deadline");
            }
        }
        if timeout {
            assert!(
                caller.await.expect("caller").is_err(),
                "HTTP wait must time out"
            );
        } else {
            caller.abort();
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(10), done_rx)
                .await
                .expect("completion deadline")
                .expect("result")
                .expect("deployment")
                .is_some()
        );
        service
            .try_acquire_process_release_lock("hotcancel")
            .await
            .expect("released ownership")
            .finish()
            .await
            .expect("finish");
        server.abort();
    }
}
#[tokio::test]
async fn readiness_is_required_and_operation_is_rechecked_after_ready() {
    use axum::{Json, Router, routing::get};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let ready = Arc::new(AtomicBool::new(false));
    let replace_on_ready = Arc::new(AtomicBool::new(false));
    let replaced = Arc::new(AtomicBool::new(false));
    let status_reads = Arc::new(AtomicUsize::new(0));
    let status_replaced = replaced.clone();
    let status_counter = status_reads.clone();
    let ready_value = ready.clone();
    let ready_replaces = replace_on_ready.clone();
    let ready_replaced = replaced.clone();
    let router = Router::new()
        .route("/v1/deploy/status", get(move || {
            let replaced = status_replaced.clone();
            let counter = status_counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({"data":{
                    "protocol_version":4,"phase":"running","operation":{
                        "operation_id":if replaced.load(Ordering::SeqCst) {"foreign"} else {"operation"},
                        "request_release_id":"request","deployment_generation_id":"generation",
                        "artifact_release_id":"artifact","deploy_stage":"succeeded","persisted":true,
                        "phase":"running","error":null
                    }
                }}))
            }
        }))
        .route("/ready", get(move || {
            let ready = ready_value.clone();
            let replace = ready_replaces.clone();
            let replaced = ready_replaced.clone();
            async move {
                if !ready.load(Ordering::SeqCst) {
                    return (axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        Json(serde_json::json!({"status":"not_ready","phase":"running"})));
                }
                if replace.load(Ordering::SeqCst) { replaced.store(true, Ordering::SeqCst); }
                (axum::http::StatusCode::OK, Json(serde_json::json!({"status":"ready","phase":"running"})))
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listen");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    let root = tempfile::tempdir().expect("root");
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root.path(), runtime.clone()).await;
    let task = super::HotDeploymentTask {
        runtime,
        access_mode: crate::config::AppAccessMode::Docker,
        app_id: "readytest".into(),
        url: "http://artifact".into(),
        release_id: "request".into(),
        sha256: String::new(),
        base: format!("http://{address}"),
        token: "token".into(),
        env_snapshot: Default::default(),
        generation_id: "generation".into(),
        operation_id: "operation".into(),
        operation: Arc::new(
            service
                .acquire_process_release_lock("readytest")
                .await
                .expect("lease"),
        ),
        budget: std::time::Duration::from_secs(300),
    };
    let client = super::admin_client().expect("client");
    assert!(
        task.read_ready_operation(&client)
            .await
            .expect("probe")
            .is_none(),
        "Running while /ready returns 503 must not complete"
    );
    assert_eq!(status_reads.load(Ordering::SeqCst), 1);
    ready.store(true, Ordering::SeqCst);
    assert!(
        task.read_ready_operation(&client)
            .await
            .expect("probe")
            .is_some()
    );
    assert_eq!(
        status_reads.load(Ordering::SeqCst),
        3,
        "successful readiness requires the second operation read"
    );
    replace_on_ready.store(true, Ordering::SeqCst);
    assert!(
        task.read_ready_operation(&client)
            .await
            .expect("probe")
            .is_none(),
        "another operation appearing after /ready must not complete this one"
    );
    assert_eq!(status_reads.load(Ordering::SeqCst), 5);
    let guard = Arc::try_unwrap(task.operation)
        .ok()
        .expect("exclusive guard");
    guard.finish().await.expect("finish");
    server.abort();
}
