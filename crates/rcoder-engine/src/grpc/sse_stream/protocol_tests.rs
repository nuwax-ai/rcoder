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
    fail_each_stream: bool,
    cursors: Arc<std::sync::Mutex<Vec<u64>>>,
    events: Arc<std::sync::Mutex<Option<Vec<grpc::ProgressEvent>>>>,
}

#[tonic::async_trait]
impl AgentService for ProtocolFixture {
    type SubscribeProgressStream = std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<grpc::ProgressEvent, Status>> + Send>,
    >;

    async fn subscribe_progress(
        &self,
        request: Request<grpc::ProgressRequest>,
    ) -> Result<Response<Self::SubscribeProgressStream>, Status> {
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
        self.cursors
            .lock()
            .unwrap()
            .push(request.into_inner().from_seq.unwrap_or_default());
        self.entered.notify_one();
        if self.hold_subscription {
            self.release.notified().await;
            return Err(Status::cancelled("controlled subscription released"));
        }
        if attempt > 0 && !self.fail_each_stream {
            return Err(Status::not_found("later_subscription_failure"));
        }
        let original = grpc::ProgressEvent {
            message_type: "SessionPromptStart".into(),
            sub_type: "prompt_start".into(),
            payload: "{}".into(),
            request_id: Some("request-original".into()),
            seq: attempt as u64 + 1,
            timestamp: (chrono::Utc::now() - chrono::Duration::seconds(1)).timestamp_millis(),
        };
        let events = self
            .events
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| vec![original]);
        let mut replies: Vec<_> = events.into_iter().map(Ok).collect();
        replies.push(Err(Status::unavailable(
            "original_stream_failure password=do-not-leak",
        )));
        Ok(Response::new(Box::pin(futures_util::stream::iter(replies))))
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
    fixture_mode(hold_subscription, false).await
}

async fn fixture_mode(
    hold_subscription: bool,
    fail_each_stream: bool,
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
        fail_each_stream,
        cursors: Arc::new(std::sync::Mutex::new(Vec::new())),
        events: Arc::new(std::sync::Mutex::new(None)),
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

struct OomRuntime {
    diagnoses: AtomicUsize,
    observation: container_runtime_api::AgentPodDiagnostic,
    delay: Duration,
    fail: bool,
}

#[tonic::async_trait]
impl container_runtime_api::AgentContainerRuntime for OomRuntime {
    async fn diagnose_agent_pod(
        &self,
        identifier: &str,
        family: &shared_types::ServiceType,
    ) -> container_runtime_api::ContainerRuntimeResult<container_runtime_api::AgentPodDiagnostic>
    {
        assert_eq!(identifier, "captured-owner");
        assert_eq!(*family, shared_types::ServiceType::ComputerAgentRunner);
        self.diagnoses.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        if self.fail {
            return Err(
                container_runtime_api::ContainerRuntimeError::ConnectionError(
                    "diagnosis unavailable".into(),
                ),
            );
        }
        Ok(self.observation.clone())
    }

    async fn create_container(
        &self,
        _: container_runtime_api::ContainerCreateParams,
    ) -> container_runtime_api::ContainerRuntimeResult<shared_types::ContainerBasicInfo> {
        Err(
            container_runtime_api::ContainerRuntimeError::ConfigurationError(
                "diagnostic-only fixture".into(),
            ),
        )
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> container_runtime_api::ContainerRuntimeResult<Option<shared_types::ContainerBasicInfo>>
    {
        Ok(None)
    }
    async fn find_container(
        &self,
        _: &str,
        _: &shared_types::ServiceType,
    ) -> container_runtime_api::ContainerRuntimeResult<
        Option<container_runtime_api::RuntimeContainerInfo>,
    > {
        Ok(None)
    }
    async fn stop_container(&self, _: &str) -> container_runtime_api::ContainerRuntimeResult<()> {
        panic!("SSE diagnosis must not stop compute")
    }
    async fn is_container_running(
        &self,
        _: &str,
    ) -> container_runtime_api::ContainerRuntimeResult<bool> {
        Ok(false)
    }
    async fn list_containers(
        &self,
    ) -> container_runtime_api::ContainerRuntimeResult<
        Vec<container_runtime_api::RuntimeContainerInfo>,
    > {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> container_runtime_api::ContainerRuntimeResult<()> {
        panic!("SSE diagnosis must not clean resources")
    }
    async fn health_check(&self) -> container_runtime_api::ContainerRuntimeResult<()> {
        Ok(())
    }
}
#[tonic::async_trait]
impl container_runtime_api::WorkspaceRuntime for OomRuntime {}
#[tonic::async_trait]
impl container_runtime_api::UserAppDeploymentRuntime for OomRuntime {}

#[tokio::test]
async fn final_stream_error_promotes_same_instance_oom_without_replacing_cause_or_request() {
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    let (address, service, stop, server) = fixture_mode(false, true).await;
    let runtime = Arc::new(OomRuntime {
        diagnoses: AtomicUsize::new(0),
        observation: oom_observation("captured-pod-uid", chrono::Utc::now()),
        delay: Duration::ZERO,
        fail: false,
    });
    let registry = Arc::new(crate::grpc::SessionStreamRegistry::new());
    let diagnostic = Arc::new(crate::utils::DiagCtx {
        expected_container_id: Some("captured-pod-uid".into()),
        runtime: runtime.clone(),
        identifier: "captured-owner".into(),
        service_type: shared_types::ServiceType::ComputerAgentRunner,
    });
    let stream = create_grpc_sse_stream(
        registry.clone(),
        address,
        "session-original".into(),
        Arc::new(crate::grpc::GrpcChannelPool::new()),
        "en-US",
        Arc::new(|_| {}),
        Some(diagnostic),
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
    .unwrap()
    .unwrap()
    .to_bytes();
    let wire = String::from_utf8(body.to_vec()).unwrap();
    let events: Vec<serde_json::Value> = wire
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|line| serde_json::from_str(line.trim()).unwrap())
        .collect();
    let failure = events.last().unwrap();
    assert_eq!(failure["subType"], "error");
    assert_eq!(failure["data"]["code"], "GRPC_SERVICE_UNAVAILABLE");
    assert_eq!(failure["data"]["request_id"], "request-original");
    assert_eq!(failure["data"]["error_detail"]["stage"], "grpc_stream");
    assert!(
        failure["data"]["message"]
            .as_str()
            .unwrap()
            .contains("out-of-memory")
    );
    assert!(
        failure["data"]["error_detail"]["detail"]
            .as_str()
            .unwrap()
            .contains("original_stream_failure")
    );
    let observed = failure["data"]["observed_container_state"]
        .as_str()
        .expect("final stream errors must use supplemental runtime diagnosis");
    assert!(
        observed.to_ascii_lowercase().contains("memory") || observed.contains("OOM"),
        "missing OOM observation: {observed}"
    );
    assert!(!wire.contains("do-not-leak"));
    assert_eq!(runtime.diagnoses.load(Ordering::SeqCst), 1);
    assert_eq!(
        *service.cursors.lock().unwrap(),
        vec![0, 1],
        "original replay cursor contract must remain intact"
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
    assert_eq!(
        failure["data"]["message"],
        shared_types::get_error_message(shared_types::ERR_AGENT_CONTAINER_UNAVAILABLE, "en-US")
    );
    assert!(
        failure["data"]["error_detail"]["detail"]
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

fn oom_observation(
    id: &str,
    finished: chrono::DateTime<chrono::Utc>,
) -> container_runtime_api::AgentPodDiagnostic {
    container_runtime_api::AgentPodDiagnostic {
        evidence: Some(container_runtime_api::AgentPodDiagnosticEvidence {
            physical_id: id.into(),
            termination_finished_at: Some(finished),
            current_termination: false,
        }),
        exists: true,
        ready: false,
        restart_count: 1,
        last_terminate_reason: Some("OOMKilled".into()),
        last_exit_code: Some(137),
        waiting_reason: Some("CrashLoopBackOff".into()),
        detail: None,
    }
}

#[tokio::test]
async fn oom_message_requires_same_physical_origin_and_current_failure_window() {
    use crate::grpc::session_stream_registry::{FailureWindow, make_terminal_error_event};
    let now = chrono::Utc::now();
    let window = FailureWindow {
        started_at: Some(now - chrono::Duration::seconds(2)),
        failed_at: now,
    };
    for (expected, physical, finished, has_time, has_origin, has_window, promoted) in [
        (
            "pod-a",
            "pod-a",
            now - chrono::Duration::seconds(1),
            true,
            true,
            true,
            true,
        ),
        (
            "pod-a",
            "pod-a",
            now - chrono::Duration::hours(1),
            true,
            true,
            true,
            false,
        ),
        (
            "pod-a",
            "pod-b",
            now - chrono::Duration::seconds(1),
            true,
            true,
            true,
            false,
        ),
        (
            "docker-old",
            "docker-new",
            now - chrono::Duration::seconds(1),
            true,
            true,
            true,
            false,
        ),
        (
            "",
            "",
            now - chrono::Duration::seconds(1),
            true,
            true,
            true,
            false,
        ),
        (
            " ",
            " ",
            now - chrono::Duration::seconds(1),
            true,
            true,
            true,
            false,
        ),
        (
            "pod-a",
            "pod-a",
            now - chrono::Duration::seconds(1),
            false,
            true,
            true,
            false,
        ),
        (
            "pod-a",
            "pod-a",
            now - chrono::Duration::seconds(1),
            true,
            false,
            true,
            false,
        ),
        (
            "pod-a",
            "pod-a",
            now - chrono::Duration::seconds(1),
            true,
            true,
            false,
            false,
        ),
        (
            "pod-a",
            "pod-a",
            now + chrono::Duration::seconds(1),
            true,
            true,
            true,
            false,
        ),
    ] {
        let mut observation = oom_observation(physical, finished);
        if !has_time {
            observation
                .evidence
                .as_mut()
                .unwrap()
                .termination_finished_at = None;
        }
        let runtime = Arc::new(OomRuntime {
            diagnoses: AtomicUsize::new(0),
            observation,
            delay: Duration::ZERO,
            fail: false,
        });
        let ctx = Arc::new(crate::utils::DiagCtx {
            expected_container_id: has_origin.then(|| expected.into()),
            runtime,
            identifier: "captured-owner".into(),
            service_type: shared_types::ServiceType::ComputerAgentRunner,
        });
        let event = make_terminal_error_event(
            Some(&ctx),
            "en-US",
            tonic::Code::Unavailable,
            "original_cause password=do-not-leak",
            "grpc_stream",
            Some("original-request".into()),
            FailureWindow {
                started_at: if has_window { window.started_at } else { None },
                ..window
            },
        )
        .await;
        let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
        assert_eq!(
            payload["message"]
                .as_str()
                .unwrap()
                .contains("out-of-memory"),
            promoted
        );
        assert_eq!(payload["code"], "GRPC_SERVICE_UNAVAILABLE");
        assert_eq!(payload["error_detail"]["stage"], "grpc_stream");
        assert!(
            payload["error_detail"]["detail"]
                .as_str()
                .unwrap()
                .contains("original_cause")
        );
        assert_eq!(event.request_id.as_deref(), Some("original-request"));
        assert_eq!(event.seq, 0);
        assert!(!event.payload.contains("do-not-leak"));
    }
}

#[tokio::test]
async fn authoritative_grpc_errors_are_not_overridden_or_diagnosed_as_oom() {
    use crate::grpc::session_stream_registry::{FailureWindow, make_terminal_error_event};
    let now = chrono::Utc::now();
    let runtime = Arc::new(OomRuntime {
        diagnoses: AtomicUsize::new(0),
        observation: oom_observation("pod-a", now),
        delay: Duration::ZERO,
        fail: false,
    });
    let ctx = Arc::new(crate::utils::DiagCtx {
        expected_container_id: Some("pod-a".into()),
        runtime: runtime.clone(),
        identifier: "captured-owner".into(),
        service_type: shared_types::ServiceType::ComputerAgentRunner,
    });
    for code in [
        tonic::Code::NotFound,
        tonic::Code::PermissionDenied,
        tonic::Code::Unauthenticated,
        tonic::Code::InvalidArgument,
        tonic::Code::ResourceExhausted,
        tonic::Code::Cancelled,
    ] {
        let event = make_terminal_error_event(
            Some(&ctx),
            "en-US",
            code,
            "authoritative_failure",
            "grpc_subscribe",
            None,
            FailureWindow {
                started_at: Some(now - chrono::Duration::seconds(1)),
                failed_at: now,
            },
        )
        .await;
        let payload: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
        assert!(
            payload["message"]
                .as_str()
                .unwrap()
                .contains("authoritative_failure")
        );
        assert!(payload.get("observed_container_state").is_none());
    }
    assert_eq!(runtime.diagnoses.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unavailable_diagnosis_and_timeout_keep_friendly_message_and_original_cause() {
    use crate::grpc::session_stream_registry::{FailureWindow, make_terminal_error_event};
    for (fail, delay) in [(true, Duration::ZERO), (false, Duration::from_secs(30))] {
        let now = chrono::Utc::now();
        let runtime = Arc::new(OomRuntime {
            diagnoses: AtomicUsize::new(0),
            observation: oom_observation("pod-a", now),
            delay,
            fail,
        });
        let ctx = Arc::new(crate::utils::DiagCtx {
            expected_container_id: Some("pod-a".into()),
            runtime,
            identifier: "captured-owner".into(),
            service_type: shared_types::ServiceType::ComputerAgentRunner,
        });
        let started = tokio::time::Instant::now();
        let event = make_terminal_error_event(
            Some(&ctx),
            "en-US",
            tonic::Code::Unavailable,
            "original_cause",
            "grpc_connect",
            None,
            FailureWindow {
                started_at: Some(now - chrono::Duration::seconds(1)),
                failed_at: now,
            },
        )
        .await;
        assert!(started.elapsed() < Duration::from_millis(2500));
        let p: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
        assert_eq!(
            p["message"],
            shared_types::get_error_message(shared_types::ERR_AGENT_CONTAINER_UNAVAILABLE, "en-US")
        );
        assert!(
            p["error_detail"]["detail"]
                .as_str()
                .unwrap()
                .contains("original_cause")
        );
        assert!(p.get("observed_container_state").is_none());
    }
}

#[tokio::test]
async fn bounded_diagnosis_can_confirm_oom_just_after_transport_failure() {
    use crate::grpc::session_stream_registry::{FailureWindow, make_terminal_error_event};
    let failed = chrono::Utc::now();
    let runtime = Arc::new(OomRuntime {
        diagnoses: AtomicUsize::new(0),
        observation: oom_observation("pod-a", failed + chrono::Duration::milliseconds(1)),
        delay: Duration::from_millis(20),
        fail: false,
    });
    let ctx = Arc::new(crate::utils::DiagCtx {
        expected_container_id: Some("pod-a".into()),
        runtime,
        identifier: "captured-owner".into(),
        service_type: shared_types::ServiceType::ComputerAgentRunner,
    });
    let event = make_terminal_error_event(
        Some(&ctx),
        "en-US",
        tonic::Code::Unavailable,
        "RST before kubelet observation",
        "grpc_stream",
        Some("original-request".into()),
        FailureWindow {
            started_at: Some(failed - chrono::Duration::seconds(1)),
            failed_at: failed,
        },
    )
    .await;
    let p: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
    assert!(p["message"].as_str().unwrap().contains("out-of-memory"));
    assert!(
        p["error_detail"]["detail"]
            .as_str()
            .unwrap()
            .contains("RST before kubelet observation")
    );
    assert_eq!(event.request_id.as_deref(), Some("original-request"));
}

#[tokio::test]
async fn cancelled_or_new_turn_does_not_inherit_previous_oom_window() {
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    for next in [
        None,
        Some("start"),
        Some("update"),
        Some("zero"),
        Some("negative"),
    ] {
        let (address, service, stop, server) = fixture_mode(false, true).await;
        let now = chrono::Utc::now();
        let event =
            |kind: &str, sub: &str, rid: &str, seq: u64, at: chrono::DateTime<chrono::Utc>| {
                grpc::ProgressEvent {
                    message_type: kind.into(),
                    sub_type: sub.into(),
                    payload: "{}".into(),
                    request_id: Some(rid.into()),
                    seq,
                    timestamp: at.timestamp_millis(),
                }
            };
        let mut events = vec![
            event(
                "SessionPromptStart",
                "prompt_start",
                "old-request",
                1,
                now - chrono::Duration::seconds(2),
            ),
            event(
                "SessionPromptEnd",
                "cancelled",
                "old-request",
                2,
                now - chrono::Duration::seconds(1),
            ),
        ];
        if let Some(kind) = next {
            let prompt_start = kind != "update";
            let at = match kind {
                "zero" => chrono::DateTime::from_timestamp_millis(0).unwrap(),
                "negative" => chrono::DateTime::from_timestamp_millis(-1).unwrap(),
                _ => now,
            };
            events.push(event(
                if prompt_start {
                    "SessionPromptStart"
                } else {
                    "AgentSessionUpdate"
                },
                if prompt_start {
                    "prompt_start"
                } else {
                    "agent_thought_chunk"
                },
                "new-request",
                3,
                at,
            ));
        }
        *service.events.lock().unwrap() = Some(events);
        let runtime = Arc::new(OomRuntime {
            diagnoses: AtomicUsize::new(0),
            observation: oom_observation("pod-a", now - chrono::Duration::seconds(1)),
            delay: Duration::ZERO,
            fail: false,
        });
        let ctx = Arc::new(crate::utils::DiagCtx {
            expected_container_id: Some("pod-a".into()),
            runtime,
            identifier: "captured-owner".into(),
            service_type: shared_types::ServiceType::ComputerAgentRunner,
        });
        let registry = Arc::new(crate::grpc::SessionStreamRegistry::new());
        let stream = create_grpc_sse_stream(
            registry.clone(),
            address,
            "session-original".into(),
            Arc::new(crate::grpc::GrpcChannelPool::new()),
            "en-US",
            Arc::new(|_| {}),
            Some(ctx),
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
        .unwrap()
        .unwrap()
        .to_bytes();
        let wire = String::from_utf8(body.to_vec()).unwrap();
        let last: serde_json::Value = serde_json::from_str(
            wire.lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .next_back()
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert_eq!(
            last["data"]["message"],
            shared_types::get_error_message(shared_types::ERR_AGENT_CONTAINER_UNAVAILABLE, "en-US")
        );
        assert_eq!(
            last["data"]["request_id"].as_str(),
            next.map(|_| "new-request")
        );
        assert!(
            last["data"]["error_detail"]["detail"]
                .as_str()
                .unwrap()
                .contains("original_stream_failure")
        );
        registry
            .drain(tokio::time::Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        stop.cancel();
        server.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn inconsistent_failure_window_only_degrades_presentation() {
    use crate::grpc::session_stream_registry::{FailureWindow, make_terminal_error_event};
    let failed = chrono::Utc::now();
    let runtime = Arc::new(OomRuntime {
        diagnoses: AtomicUsize::new(0),
        observation: oom_observation("pod-a", failed + chrono::Duration::milliseconds(2)),
        delay: Duration::from_millis(20),
        fail: false,
    });
    let ctx = Arc::new(crate::utils::DiagCtx {
        expected_container_id: Some("pod-a".into()),
        runtime,
        identifier: "captured-owner".into(),
        service_type: shared_types::ServiceType::ComputerAgentRunner,
    });
    let event = make_terminal_error_event(
        Some(&ctx),
        "en-US",
        tonic::Code::Unavailable,
        "original_cause",
        "grpc_stream",
        Some("original-request".into()),
        FailureWindow {
            started_at: Some(failed + chrono::Duration::milliseconds(1)),
            failed_at: failed,
        },
    )
    .await;
    let p: serde_json::Value = serde_json::from_str(&event.payload).unwrap();
    assert_eq!(
        p["message"],
        shared_types::get_error_message(shared_types::ERR_AGENT_CONTAINER_UNAVAILABLE, "en-US")
    );
    assert_eq!(event.request_id.as_deref(), Some("original-request"));
}
