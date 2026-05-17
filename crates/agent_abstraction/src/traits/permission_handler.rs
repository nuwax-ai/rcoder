use agent_client_protocol::Responder;
use agent_client_protocol::schema::{
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use anyhow::Result;
use async_trait::async_trait;

/// Runtime context for an ACP permission request.
#[derive(Debug, Clone)]
pub struct PermissionRequestContext {
    pub project_id: String,
    pub user_id: Option<String>,
    pub agent_mode: shared_types::AgentMode,
    pub service_type: shared_types::ServiceType,
    pub request_id: Option<String>,
}

/// Handles ACP permission requests without coupling agent_abstraction to agent_runner.
#[async_trait]
pub trait PermissionRequestHandler: Send + Sync + 'static {
    async fn handle_permission_request(
        &self,
        context: PermissionRequestContext,
        request: RequestPermissionRequest,
        responder: Responder<RequestPermissionResponse>,
    ) -> Result<(), agent_client_protocol::Error>;
}

/// Default handler used outside agent_runner. It preserves the historical YOLO behavior.
#[derive(Debug, Clone, Default)]
pub struct YoloPermissionRequestHandler;

#[async_trait]
impl PermissionRequestHandler for YoloPermissionRequestHandler {
    async fn handle_permission_request(
        &self,
        _context: PermissionRequestContext,
        request: RequestPermissionRequest,
        responder: Responder<RequestPermissionResponse>,
    ) -> Result<(), agent_client_protocol::Error> {
        let selected = request
            .options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowAlways)
            .or_else(|| {
                request
                    .options
                    .iter()
                    .find(|o| o.kind == PermissionOptionKind::AllowOnce)
            })
            .or_else(|| request.options.first());

        if let Some(option) = selected {
            responder.respond(RequestPermissionResponse::new(
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    option.option_id.clone(),
                )),
            ))
        } else {
            responder.respond(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ))
        }
    }
}
