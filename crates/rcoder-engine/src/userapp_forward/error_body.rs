//! Bounded disposal only after rejecting a request that was never forwarded.
//! These limits are not upload, download, extraction, or artifact quotas.
use axum::{body::Body, extract::Request, response::Response};
use futures_util::StreamExt as _;
use std::time::Duration;

const ERROR_DRAIN_BYTES: usize = 1024 * 1024;
const ERROR_DRAIN_TIME: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainOutcome {
    Complete,
    ByteBudget,
    Deadline,
    BodyError,
}

async fn drain(body: Body, byte_budget: usize, time_budget: Duration) -> DrainOutcome {
    let consume = async {
        let mut stream = body.into_data_stream();
        let mut remaining = byte_budget;
        while remaining > 0 {
            match stream.next().await {
                Some(Ok(bytes)) if bytes.len() >= remaining => return DrainOutcome::ByteBudget,
                Some(Ok(bytes)) => remaining -= bytes.len(),
                Some(Err(_)) => return DrainOutcome::BodyError,
                None => return DrainOutcome::Complete,
            }
        }
        DrainOutcome::ByteBudget
    };
    match tokio::time::timeout(time_budget, consume).await {
        Ok(outcome) => outcome,
        Err(_) => DrainOutcome::Deadline,
    }
}

pub(super) async fn reject(req: Request, response: Response) -> Response {
    let outcome = drain(req.into_body(), ERROR_DRAIN_BYTES, ERROR_DRAIN_TIME).await;
    if outcome != DrainOutcome::Complete {
        tracing::debug!(
            ?outcome,
            "Rejected userApp request body disposal ended at its boundary"
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn error_cleanup_handles_complete_excessive_and_stalled_bodies() {
        assert_eq!(
            drain(Body::from("small"), 10, Duration::from_secs(1)).await,
            DrainOutcome::Complete
        );
        assert_eq!(
            drain(Body::from("too-large"), 3, Duration::from_secs(1)).await,
            DrainOutcome::ByteBudget
        );
        let failed = futures_util::stream::once(async {
            Err::<axum::body::Bytes, _>(std::io::Error::other("body failure"))
        });
        assert_eq!(
            drain(Body::from_stream(failed), 10, Duration::from_secs(1)).await,
            DrainOutcome::BodyError
        );
        let pending = futures_util::stream::pending::<Result<axum::body::Bytes, std::io::Error>>();
        assert_eq!(
            drain(Body::from_stream(pending), 10, Duration::from_millis(20)).await,
            DrainOutcome::Deadline
        );
    }
    #[tokio::test]
    async fn small_slow_multipart_receives_original_envelope_over_real_http() {
        use axum::response::IntoResponse as _;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let app = axum::Router::new().route(
            "/upload",
            axum::routing::post(|req: Request| async move {
                let response = axum::Json(
                    serde_json::json!({"code":"ERR_BACKEND_ERROR","message":"Builder unavailable"}),
                )
                .into_response();
                reject(req, response).await
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            let mut client = tokio::net::TcpStream::connect(address).await.expect("connect");
            let body = "--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nfile content\r\n--boundary--\r\n";
            let headers = format!("POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary=boundary\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            client.write_all(headers.as_bytes()).await.expect("headers");
            for chunk in body.as_bytes().chunks(16) {
                client.write_all(chunk).await.expect("slow body write");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let mut response = String::new();
            client.read_to_string(&mut response).await.expect("response without reset");
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            assert!(response.contains("ERR_BACKEND_ERROR"));
        }).await;
        server.abort();
        assert!(server.await.expect_err("server aborted").is_cancelled());
        result.expect("bounded HTTP test");
    }
    #[tokio::test]
    async fn stalled_http_upload_releases_rejection_handler_within_budget() {
        use axum::response::IntoResponse as _;
        use tokio::io::AsyncWriteExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let (completed, observed) = tokio::sync::oneshot::channel();
        let completed = std::sync::Arc::new(tokio::sync::Mutex::new(Some(completed)));
        let app = axum::Router::new().route(
            "/upload",
            axum::routing::post(move |req: Request| {
                let completed = completed.clone();
                async move {
                    let response = reject(
                        req,
                        axum::Json(serde_json::json!({
                            "code": "ERR_BACKEND_ERROR", "message": "Builder unavailable"
                        }))
                        .into_response(),
                    )
                    .await;
                    if let Some(sender) = completed.lock().await.take() {
                        let _ = sender.send(());
                    }
                    response
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let result = tokio::time::timeout(Duration::from_secs(4), async {
            let mut client = tokio::net::TcpStream::connect(address).await.expect("connect");
            client.write_all(b"POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Type: multipart/form-data; boundary=b\r\nContent-Length: 10000000\r\n\r\n--b\r\n")
                .await.expect("partial request");
            // Keep the socket open with an incomplete body. Handler completion,
            // not successful transport delivery, is the stalled-upload contract.
            observed.await.expect("rejection handler completed");
            drop(client);
        }).await;
        server.abort();
        assert!(server.await.expect_err("server aborted").is_cancelled());
        result.expect("stalled upload must not retain its handler");
    }
}
