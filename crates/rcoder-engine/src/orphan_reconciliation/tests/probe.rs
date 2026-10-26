//! Real gRPC capability contract; no model execution is involved.
use super::super::*;
use shared_types::grpc::{
    self,
    agent_service_server::{AgentService, AgentServiceServer},
};
use tonic::{Request, Response, Status};

struct Agent {
    version: u32,
    active: bool,
}
#[tonic::async_trait]
impl AgentService for Agent {
    type SubscribeProgressStream = std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<grpc::ProgressEvent, Status>> + Send>,
    >;
    async fn get_container_status(
        &self,
        _: Request<grpc::GetContainerStatusRequest>,
    ) -> Result<Response<grpc::GetContainerStatusResponse>, Status> {
        Ok(Response::new(grpc::GetContainerStatusResponse {
            idle_observation_version: self.version,
            is_active: self.active,
            ..Default::default()
        }))
    }
    async fn subscribe_progress(
        &self,
        _: Request<grpc::ProgressRequest>,
    ) -> Result<Response<Self::SubscribeProgressStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn chat(
        &self,
        _: Request<grpc::ChatRequest>,
    ) -> Result<Response<grpc::ChatResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn cancel_session(
        &self,
        _: Request<grpc::CancelRequest>,
    ) -> Result<Response<grpc::CancelResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn resolve_permission(
        &self,
        _: Request<grpc::ResolvePermissionRequest>,
    ) -> Result<Response<grpc::ResolvePermissionResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn get_status(
        &self,
        _: Request<grpc::GetStatusRequest>,
    ) -> Result<Response<grpc::GetStatusResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn stop_agent(
        &self,
        _: Request<grpc::StopAgentRequest>,
    ) -> Result<Response<grpc::StopAgentResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn get_vnc_status(
        &self,
        _: Request<grpc::GetVncStatusRequest>,
    ) -> Result<Response<grpc::GetVncStatusResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
}

/// 产品决策（2026-10-26）：旧 agent（证据版本 0）与新 agent 同等参与自动
/// 回收——判定只依赖探活返回的 is_active/active_tasks。修复前 version=0
/// 一律返回 Err，存量旧容器即使长期闲置也永不回收。
#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn activity_probe_accepts_legacy_activity_reports() {
    use tokio_stream::wrappers::TcpListenerStream;
    for (version, active, expect) in [
        (0, false, "idle"),
        (0, true, "busy"),
        (1, false, "idle"),
        (1, true, "busy"),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(AgentServiceServer::new(Agent { version, active }))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        let (scanner, _, _, _directory) = super::fixture().await;
        let probe = GrpcContainerActivityProbe {
            pool: scanner.state.grpc_pool.clone(),
        };
        let mut snapshot = super::docker_web_agent();
        snapshot.endpoint = Some(format!("http://{addr}"));
        let result = probe.probe(&snapshot).await;
        match expect {
            "idle" => assert!(matches!(result, Ok(Activity::Idle))),
            "busy" => assert!(matches!(result, Ok(Activity::Busy))),
            _ => unreachable!(),
        }
        task.abort();
        drop(task.await);
    }
}
