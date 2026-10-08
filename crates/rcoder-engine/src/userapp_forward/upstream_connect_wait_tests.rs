//! dev 转发连接等待与发送失败分类的协议级回归（真实 socket，无 K8s）。
use super::dev_connect::{DevConnectionObservation, observe_dev_connection_until};
use super::*;
use axum::http::StatusCode;

fn restarting_operation() -> shared_types::UserAppOperationView {
    shared_types::UserAppOperationView {
        operation_id: "original-restart".into(),
        app_id: "fixtureapp".into(),
        lifecycle_id: "lifecycle".into(),
        request_id: None,
        kind: shared_types::UserAppOperationKind::RestartBuilder,
        scope: shared_types::UserAppOperationScope::Dev,
        state: shared_types::UserAppOperationState::Running,
        revision: 1,
        step: "starting".into(),
        error_code: None,
        error_message: None,
        created_at: chrono::Utc::now(),
    }
}

#[tokio::test(start_paused = true)]
async fn connect_wait_refreshes_finished_operation_and_preserves_query_failure() {
    for query_fails in [false, true] {
        let reads = std::cell::Cell::new(0);
        let observed = observe_dev_connection_until(
            tokio::time::Instant::now() + std::time::Duration::from_millis(750),
            || std::future::ready(false),
            || {
                reads.set(reads.get() + 1);
                std::future::ready(if reads.get() == 1 {
                    Ok(vec![restarting_operation()])
                } else if query_fails {
                    Err(HttpResultError::bad_gateway(
                        "operation storage unavailable",
                    ))
                } else {
                    // The control head no longer holds this finished operation.
                    Ok(vec![])
                })
            },
        )
        .await;
        assert!(reads.get() >= 2, "must refresh the first Running snapshot");
        if query_fails {
            let response = observed
                .expect_err("observation failure is not absence")
                .into_response();
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        } else {
            assert!(
                matches!(
                    observed.unwrap(),
                    DevConnectionObservation::Unreachable(None)
                ),
                "a finished operation must not be reported as still running"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn connect_wait_rejects_ready_result_after_deadline() {
    let observed = observe_dev_connection_until(
        tokio::time::Instant::now() + std::time::Duration::from_millis(10),
        || async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            true
        },
        || std::future::ready(Ok(vec![])),
    )
    .await
    .unwrap();
    assert!(matches!(
        observed,
        DevConnectionObservation::Unreachable(None)
    ));
}

/// 事故反例回归（2026-10-08 app 221）：builder 重启窗口内连接失败——
/// 不得把裸 reqwest 错误（含集群内部 svc 地址）直通用户；连接类失败
/// 分类为 ERR_CONTAINER_ADDRESS_NOT_READY + Retry-After + 本地化文案。
#[tokio::test]
async fn dev_forward_connect_failure_is_classified_not_raw() {
    // 绑定后立即释放：拿到一个几乎必然拒绝连接的端口。
    let address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap()
    };
    let request = Request::builder()
        .uri("/api/v1/userapp/dev/restart")
        .method("POST")
        .body(Body::empty())
        .unwrap();
    let response = forward_to_addr(
        ForwardKind::Dev,
        "fixture-221",
        &format!("http://{address}"),
        &shared_types::FileServerRequestCredentials::default(),
        request,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("10")
    );
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        payload["code"],
        shared_types::error_codes::ERR_CONTAINER_ADDRESS_NOT_READY
    );
    let message = payload["message"].as_str().unwrap_or_default();
    assert!(
        !message.contains("error sending request")
            && !message.contains("http://")
            && !message.is_empty(),
        "连接失败文案必须分类净化（非空、无 reqwest 原文/内部地址）: {message}"
    );
}

/// 预检核：端口可连立即 true；不可连在 deadline 内 false（供
/// wait_for_dev_service 的循环复用语义）。
#[tokio::test]
async fn tcp_connect_once_within_bounds() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let _server = tokio::spawn(async move {
        // 保持监听存活即可；连接由对端建立后立即结束测试。
        while let Ok((_socket, _)) = listener.accept().await {}
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    assert!(tcp_connect_once_within("127.0.0.1", address.port(), deadline).await);

    let refused = {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        probe.local_addr().unwrap()
    };
    let tight_deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(50);
    assert!(
        !tcp_connect_once_within("127.0.0.1", refused.port(), tight_deadline).await,
        "拒绝连接的端口在紧预算内应返回 false"
    );
}
