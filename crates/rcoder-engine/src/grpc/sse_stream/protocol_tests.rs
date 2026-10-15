//! Controlled gRPC protocol fixtures, not an AI or deployment E2E result.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use futures_util::StreamExt;
use shared_types::grpc::{
    self,
    agent_service_server::{AgentService, AgentServiceServer},
};
use tonic::{Request, Response, Status};

use super::create_grpc_sse_stream;

#[derive(Clone)]
struct ProtocolFixture {
    calls: Arc<AtomicUsize>,
    hold_subscription: bool,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[tonic::async_trait]
impl AgentService for ProtocolFixture {
    type SubscribeProgressStream = std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<grpc::ProgressEvent, Status>> + Send>,
    >;

    async fn subscribe_progress(
        &self,
        _: Request<grpc::ProgressRequest>,
    ) -> Result<Response<Self::SubscribeProgressStream>, Status> {
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self.hold_subscription {
            self.release.notified().await;
            return Err(Status::cancelled("controlled subscription released"));
        }
        if attempt > 0 {
            return Err(Status::not_found("later_subscription_failure"));
        }
        let original = grpc::ProgressEvent {
            message_type: "SessionPromptStart".into(),
            sub_type: "prompt_start".into(),
            payload: "{}".into(),
            request_id: Some("request-original".into()),
            seq: 1,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };
        Ok(Response::new(Box::pin(futures_util::stream::iter(vec![
            Ok(original),
            Err(Status::unavailable(
                "original_stream_failure password=do-not-leak",
            )),
        ]))))
    }
    async fn chat(
        &self,
        _: Request<grpc::ChatRequest>,
    ) -> Result<Response<grpc::ChatResponse>, Status> {
        Err(Status::unimplemented(
            "protocol fixture does not execute AI",
        ))
    }
    async fn cancel_session(
        &self,
        _: Request<grpc::CancelRequest>,
    ) -> Result<Response<grpc::CancelResponse>, Status> {
        Err(Status::unimplemented("unused fixture endpoint"))
    }
    async fn resolve_permission(
        &self,
        _: Request<grpc::ResolvePermissionRequest>,
    ) -> Result<Response<grpc::ResolvePermissionResponse>, Status> {
        Err(Status::unimplemented("unused fixture endpoint"))
    }
    async fn get_status(
        &self,
        _: Request<grpc::GetStatusRequest>,
    ) -> Result<Response<grpc::GetStatusResponse>, Status> {
        Err(Status::unimplemented("unused fixture endpoint"))
    }
    async fn stop_agent(
        &self,
        _: Request<grpc::StopAgentRequest>,
    ) -> Result<Response<grpc::StopAgentResponse>, Status> {
        Err(Status::unimplemented("unused fixture endpoint"))
    }
    async fn get_container_status(
        &self,
        _: Request<grpc::GetContainerStatusRequest>,
    ) -> Result<Response<grpc::GetContainerStatusResponse>, Status> {
        Err(Status::unimplemented("unused fixture endpoint"))
    }
    async fn get_vnc_status(
        &self,
        _: Request<grpc::GetVncStatusRequest>,
    ) -> Result<Response<grpc::GetVncStatusResponse>, Status> {
        Err(Status::unimplemented("unused fixture endpoint"))
    }
}

async fn fixture(
    hold_subscription: bool,
) -> (
    String,
    ProtocolFixture,
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
) {
    let service = ProtocolFixture {
        calls: Arc::new(AtomicUsize::new(0)),
        hold_subscription,
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let incoming = futures_util::stream::unfold(listener, |listener| async {
        let socket = listener.accept().await.map(|(socket, _)| socket);
        Some((socket, listener))
    });
    let stop = tokio_util::sync::CancellationToken::new();
    let stop_for_task = stop.clone();
    let service_for_task = service.clone();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(AgentServiceServer::new(service_for_task))
            .serve_with_incoming_shutdown(incoming, stop_for_task.cancelled())
            .await
    });
    (address, service, stop, server)
}

#[tokio::test]
async fn sse_error_retains_original_cause_and_request_after_later_subscription_failure() {
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    let (address, service, stop, server) = fixture(false).await;
    let registry = Arc::new(crate::grpc::SessionStreamRegistry::new());
    let stream = create_grpc_sse_stream(
        registry.clone(),
        address,
        "session-original".into(),
        Arc::new(crate::grpc::GrpcChannelPool::new()),
        "en-US",
        Arc::new(|_| {}),
        None,
        0,
    )
    .await;
    let body = tokio::time::timeout(
        Duration::from_secs(3),
        axum::response::Sse::new(stream)
            .into_response()
            .into_body()
            .collect(),
    )
    .await
    .expect("subscription must reach its error event")
    .unwrap()
    .to_bytes();
    let wire = String::from_utf8(body.to_vec()).unwrap();
    let events: Vec<serde_json::Value> = wire
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|json| serde_json::from_str(json.trim()).unwrap())
        .collect();
    assert_eq!(
        service.calls.load(Ordering::SeqCst),
        2,
        "must exercise real retry"
    );
    assert_eq!(events[0]["data"]["request_id"], "request-original");
    let failure = events.last().unwrap();
    assert_eq!(failure["subType"], "error");
    assert_eq!(failure["data"]["code"], "GRPC_SERVICE_UNAVAILABLE");
    assert_eq!(failure["data"]["error_detail"]["stage"], "grpc_stream");
    assert_eq!(failure["data"]["request_id"], "request-original");
    assert_eq!(failure["data"]["error_detail"]["retryable"], false);
    assert!(
        failure["data"]["message"]
            .as_str()
            .unwrap()
            .contains("original_stream_failure")
    );
    assert!(
        !wire.contains("later_subscription_failure"),
        "later retry must not replace original cause"
    );
    assert!(
        !wire.contains("do-not-leak"),
        "transport diagnostics must redact credentials"
    );
    assert!(
        failure["data"].get("operation_id").is_none(),
        "protocol has no operation identity; do not fabricate one"
    );
    registry
        .drain(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn sse_shutdown_cancels_pending_subscribe_rpc() {
    let (address, service, stop, server) = fixture(true).await;
    let registry = Arc::new(crate::grpc::SessionStreamRegistry::new());
    let stream = create_grpc_sse_stream(
        registry.clone(),
        address,
        "session-original".into(),
        Arc::new(crate::grpc::GrpcChannelPool::new()),
        "en-US",
        Arc::new(|_| {}),
        None,
        0,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(2), service.entered.notified())
        .await
        .unwrap();
    assert!(registry.shutdown_session("session-original"));
    tokio::pin!(stream);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), stream.next())
            .await
            .expect("shutdown must interrupt SubscribeProgress before its headers arrive")
            .is_none()
    );
    registry
        .drain(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    service.release.notify_one();
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
