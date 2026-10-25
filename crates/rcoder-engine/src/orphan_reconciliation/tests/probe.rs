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

/// 运维核对准入的精确 CID：旧 agent（version=0）在准入后才可被观察为
/// Idle；忙 agent 即便准入也保持 Busy；未准入 CID 维持拒绝。
#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn admitted_exact_cid_waives_only_the_evidence_version_gate() {
    use tokio_stream::wrappers::TcpListenerStream;
    let admitted: std::collections::HashSet<String> =
        ["cid-legacy-0001".to_string()].into_iter().collect();
    for (version, active, expect) in [(0, false, "idle"), (0, true, "busy"), (1, false, "idle")] {
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
            legacy_cid_admission: admitted.clone(),
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

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn orphan_probe_requires_full_activity_contract_and_preserves_busy_agents() {
    use tokio_stream::wrappers::TcpListenerStream;
    for (version, active) in [(0, false), (1, false), (1, true)] {
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
            legacy_cid_admission: std::collections::HashSet::new(),
        };
        let mut snapshot = super::docker_web_agent();
        snapshot.endpoint = Some(format!("http://{addr}"));
        let result = probe.probe(&snapshot).await;
        if version == 0 {
            assert!(result.is_err());
        } else if active {
            assert!(matches!(result, Ok(Activity::Busy)));
        } else {
            assert!(matches!(result, Ok(Activity::Idle)));
        }
        task.abort();
        drop(task.await);
    }
}
