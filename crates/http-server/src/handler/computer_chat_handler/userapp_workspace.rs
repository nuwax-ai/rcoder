//! The workspace phase used by UserApp development chat.
use super::ChatFlowExit;
use shared_types::{FileServerRequestCredentials, WakeFailure};

pub(super) async fn ensure(
    address: &str,
    app_id: &str,
    credentials: Result<FileServerRequestCredentials, WakeFailure>,
    locale: &str,
) -> Result<(), ChatFlowExit> {
    let credentials = credentials
        .map_err(|error| ChatFlowExit::response(error.into_app_error().into_http_result(locale)))?;
    crate::userapp_forward::ensure_workspace_via_dev(address, app_id, &credentials)
        .await
        .map_err(|error| {
            tracing::error!("[USERAPP_DEV_CHAT] {error}: app_id={app_id}");
            ChatFlowExit::response(error.into_http_result(locale))
        })
}

#[cfg(test)]
mod tests {
    use super::{ChatFlowExit, FileServerRequestCredentials, ensure};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn dev_chat_workspace_reaches_optional_file_token_peer_and_keeps_typed_failure() {
        for (required, token, typed_failure, success) in [
            (false, None, false, true),
            (true, Some("chat-file-token"), false, true),
            (true, None, false, false),
            (true, Some("wrong-token"), false, false),
            (true, Some("chat-file-token"), true, false),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let peer_calls = calls.clone();
            let peer=Router::new().route("/api/v1/userapp/ensure-workspace",post(move |headers:HeaderMap,Json(request):Json<serde_json::Value>| {
                let calls=peer_calls.clone();
                async move {
                    calls.fetch_add(1,Ordering::SeqCst);
                    assert!(!headers.contains_key("x-api-key"),"main control key does not belong to this peer");
                    assert_eq!(request["app_id"],"chatapp");
                    if required && headers.get("x-proxy-token").and_then(|v|v.to_str().ok())!=Some("chat-file-token") {
                        return (StatusCode::UNAUTHORIZED,Json(serde_json::json!({"success":false,"code":"ERR_RUNTIME_CONFIGURATION","message":"configured file token was rejected"})));
                    }
                    if typed_failure {
                        (StatusCode::OK,Json(serde_json::json!({
                            "success":false,"code":"ERR_DATABASE_NOT_READY","message":"Database is not ready",
                            "operation_id":"original-chat-workspace-operation",
                            "blocker":{"scope":"Dev","operation_id":"original-blocking-builder-stop","kind":"StopBuilder","state":"Running","step":"stop"},
                            "error_detail":{"reason_code":"ERR_DATABASE_NOT_READY","stage":"original_chat_workspace","detail":"Database is not ready",
                                "hint":"Inspect the existing task","retryable":true,"task_id":"original-chat-workspace-task","service_id":"postgres"}
                        })))
                    } else {
                        (StatusCode::OK,Json(serde_json::json!({"success":true})))
                    }
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, peer).await.unwrap();
            });
            let outcome = ensure(
                &address,
                "chatapp",
                Ok(FileServerRequestCredentials {
                    proxy_token: token.map(str::to_owned),
                }),
                "en-US",
            )
            .await;
            assert_eq!(outcome.is_ok(), success);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "no replay of an explicit peer rejection"
            );
            if typed_failure {
                let response = match outcome {
                    Err(ChatFlowExit::Response(response)) => response,
                    _ => panic!("business error envelope"),
                };
                assert!(!response.success);
                assert_eq!(response.code, shared_types::ERR_DATABASE_NOT_READY);
                assert_eq!(
                    response.operation_id.as_deref(),
                    Some("original-chat-workspace-operation")
                );
                assert_eq!(
                    response.blocker.unwrap().operation_id,
                    "original-blocking-builder-stop"
                );
                let detail = response.error_detail.unwrap();
                assert_eq!(detail.stage, "original_chat_workspace");
                assert_eq!(
                    detail.task_id.as_deref(),
                    Some("original-chat-workspace-task")
                );
            }
            server.abort();
            drop(server.await);
        }
    }
}
