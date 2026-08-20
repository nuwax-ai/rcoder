//! gRPC 服务模块
//!
//! 提供 agent_runner 的 gRPC 服务端实现，用于替代原有的 HTTP 接口

mod cancel;
mod chat;
mod cleanup;
mod conversion;
mod locale;
mod permission;
mod status;
mod stop_agent;
mod subscribe_progress;
mod trace_ctx;
pub(crate) mod utils;
pub(crate) mod vnc_probe;

// HTTP 停止路径复用 gRPC 的清理逻辑（含 graceful_stop），故 crate 内重导出
pub(crate) use cleanup::remove_agent_and_cleanup;

use std::sync::Arc;

use shared_types::grpc::{
    CancelRequest, CancelResponse, ChatRequest as GrpcChatRequest,
    ChatResponse as GrpcChatResponse, GetContainerStatusRequest, GetContainerStatusResponse,
    GetStatusRequest, GetStatusResponse, GetVncStatusRequest, GetVncStatusResponse,
    ProgressRequest, ResolvePermissionRequest as GrpcResolvePermissionRequest,
    ResolvePermissionResponse as GrpcResolvePermissionResponse, StopAgentRequest,
    StopAgentResponse, agent_service_server::AgentService,
};
use tonic::{Request, Response, Status};
use tracing::Instrument;

use crate::router::AppState;

pub struct AgentServiceImpl {
    app_state: Arc<AppState>,
}

impl AgentServiceImpl {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }
}

#[tonic::async_trait]
impl AgentService for AgentServiceImpl {
    async fn chat(
        &self,
        request: Request<GrpcChatRequest>,
    ) -> Result<Response<GrpcChatResponse>, Status> {
        // trace_ctx::in_span：span 创建期挂接 traceparent（见 trace_ctx.rs 注释）
        let span = rcoder_telemetry::grpc_span!("chat", request.metadata());
        chat::chat(&self.app_state, request).instrument(span).await
    }

    type SubscribeProgressStream = subscribe_progress::SubscribeProgressStream;

    async fn subscribe_progress(
        &self,
        request: Request<ProgressRequest>,
    ) -> Result<Response<Self::SubscribeProgressStream>, Status> {
        let span = rcoder_telemetry::grpc_span!("subscribe_progress", request.metadata());
        subscribe_progress::subscribe_progress(&self.app_state, request)
            .instrument(span)
            .await
    }

    async fn cancel_session(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        let span = rcoder_telemetry::grpc_span!("cancel_session", request.metadata());
        cancel::cancel_session(&self.app_state, request)
            .instrument(span)
            .await
    }

    async fn resolve_permission(
        &self,
        request: Request<GrpcResolvePermissionRequest>,
    ) -> Result<Response<GrpcResolvePermissionResponse>, Status> {
        let span = rcoder_telemetry::grpc_span!("resolve_permission", request.metadata());
        permission::resolve_permission(request).instrument(span).await
    }

    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let span = rcoder_telemetry::grpc_span!("get_status", request.metadata());
        status::get_status(&self.app_state, request).instrument(span).await
    }

    async fn stop_agent(
        &self,
        request: Request<StopAgentRequest>,
    ) -> Result<Response<StopAgentResponse>, Status> {
        let span = rcoder_telemetry::grpc_span!("stop_agent", request.metadata());
        stop_agent::stop_agent(&self.app_state, request)
            .instrument(span)
            .await
    }

    async fn get_container_status(
        &self,
        request: Request<GetContainerStatusRequest>,
    ) -> Result<Response<GetContainerStatusResponse>, Status> {
        // 后台状态探测无 traceparent（rcoder 有意不注入）——独立根 span
        // 后台状态探测无 traceparent（rcoder 有意不注入）——独立根 span
        let span = rcoder_telemetry::grpc_span!("get_container_status", request.metadata());
        status::get_container_status(&self.app_state, request)
            .instrument(span)
            .await
    }

    async fn get_vnc_status(
        &self,
        request: Request<GetVncStatusRequest>,
    ) -> Result<Response<GetVncStatusResponse>, Status> {
        let span = rcoder_telemetry::grpc_span!("get_vnc_status", request.metadata());
        status::get_vnc_status(&self.app_state, request)
            .instrument(span)
            .await
    }
}
