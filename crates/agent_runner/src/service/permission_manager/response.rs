use agent_client_protocol::Responder;
use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};

pub(super) fn respond_with_preferred_option(
    request: &RequestPermissionRequest,
    responder: Responder<RequestPermissionResponse>,
    preferred: &[PermissionOptionKind],
) -> Result<(), agent_client_protocol::Error> {
    let selected = select_option(&request.options, preferred).or_else(|| request.options.first());
    if let Some(option) = selected {
        responder.respond(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            )),
        ))
    } else {
        responder.respond(cancelled_response())
    }
}

pub(super) fn select_option<'a>(
    options: &'a [PermissionOption],
    preferred: &[PermissionOptionKind],
) -> Option<&'a PermissionOption> {
    for kind in preferred {
        if let Some(option) = options.iter().find(|option| option.kind == *kind) {
            return Some(option);
        }
    }
    None
}

pub(super) fn cancelled_response() -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
}
