//! ResolvePermission RPC 实现

use shared_types::grpc::{
    ResolvePermissionRequest as GrpcResolvePermissionRequest,
    ResolvePermissionResponse as GrpcResolvePermissionResponse,
};
use tonic::{Request, Response, Status};
use tracing::{info, instrument};

use crate::service::PERMISSION_MANAGER;

#[instrument(skip(request))]
pub async fn resolve_permission(
    request: Request<GrpcResolvePermissionRequest>,
) -> Result<Response<GrpcResolvePermissionResponse>, Status> {
    let req = request.into_inner();
    info!(
        "[gRPC] ResolvePermission: session_id={}, tool_call_id={}, project_id={}, cancelled={}, save_rule={}",
        req.session_id, req.tool_call_id, req.project_id, req.cancelled, req.save_rule
    );

    if req.session_id.trim().is_empty() || req.tool_call_id.trim().is_empty() {
        return Err(Status::invalid_argument(
            "session_id and tool_call_id are required",
        ));
    }

    let dto = shared_types::ResolvePermissionRequestDto {
        session_id: req.session_id,
        tool_call_id: req.tool_call_id,
        option_id: req.option_id,
        cancelled: req.cancelled,
        save_rule: req.save_rule,
        project_id: Some(req.project_id).filter(|s| !s.trim().is_empty()),
        user_id: req.user_id,
        pod_id: req.pod_id,
        tenant_id: req.tenant_id,
        space_id: req.space_id,
        isolation_type: req.isolation_type,
    };

    let result = PERMISSION_MANAGER.resolve_permission(dto).await;
    Ok(Response::new(GrpcResolvePermissionResponse {
        success: result.success,
        session_id: result.session_id,
        tool_call_id: result.tool_call_id,
        outcome_json: result.outcome_json,
        rule_saved: result.rule_saved,
        error_code: result.error_code,
        message: result.message,
    }))
}
