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
use tracing::instrument;

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
    #[instrument(skip(self, request), fields(trace_id = tracing::field::Empty))]
    async fn chat(
        &self,
        request: Request<GrpcChatRequest>,
    ) -> Result<Response<GrpcChatResponse>, Status> {
        trace_ctx::attach_trace_parent(&request);
        chat::chat(&self.app_state, request).await
    }

    type SubscribeProgressStream = subscribe_progress::SubscribeProgressStream;

    #[instrument(skip(self, request), fields(trace_id = tracing::field::Empty))]
    async fn subscribe_progress(
        &self,
        request: Request<ProgressRequest>,
    ) -> Result<Response<Self::SubscribeProgressStream>, Status> {
        trace_ctx::attach_trace_parent(&request);
        subscribe_progress::subscribe_progress(&self.app_state, request).await
    }

    #[instrument(skip(self, request), fields(trace_id = tracing::field::Empty))]
    async fn cancel_session(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        trace_ctx::attach_trace_parent(&request);
        cancel::cancel_session(&self.app_state, request).await
    }

    #[instrument(skip(self, request), fields(trace_id = tracing::field::Empty))]
    async fn resolve_permission(
        &self,
        request: Request<GrpcResolvePermissionRequest>,
    ) -> Result<Response<GrpcResolvePermissionResponse>, Status> {
        trace_ctx::attach_trace_parent(&request);
        permission::resolve_permission(request).await
    }

    #[instrument(skip(self, request), fields(trace_id = tracing::field::Empty))]
    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        trace_ctx::attach_trace_parent(&request);
        status::get_status(&self.app_state, request).await
    }

    #[instrument(skip(self, request), fields(trace_id = tracing::field::Empty))]
    async fn stop_agent(
        &self,
        request: Request<StopAgentRequest>,
    ) -> Result<Response<StopAgentResponse>, Status> {
        trace_ctx::attach_trace_parent(&request);
        stop_agent::stop_agent(&self.app_state, request).await
    }

    #[instrument(skip(self, request), fields(trace_id = tracing::field::Empty))]
    async fn get_container_status(
        &self,
        request: Request<GetContainerStatusRequest>,
    ) -> Result<Response<GetContainerStatusResponse>, Status> {
        // 后台状态探测无 traceparent（rcoder 有意不注入）——no-op，保持统一入口
        trace_ctx::attach_trace_parent(&request);
        status::get_container_status(&self.app_state, request).await
    }

    #[instrument(skip(self, request), fields(trace_id = tracing::field::Empty))]
    async fn get_vnc_status(
        &self,
        request: Request<GetVncStatusRequest>,
    ) -> Result<Response<GetVncStatusResponse>, Status> {
        trace_ctx::attach_trace_parent(&request);
        status::get_vnc_status(&self.app_state, request).await
    }
}
