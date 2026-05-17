use agent_client_protocol::schema::{RequestPermissionOutcome, RequestPermissionResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Nested body matching the design doc's `permission_resolve_request` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionResolveRequest {
    /// Full ACP `RequestPermissionResponse` including the tagged outcome.
    pub request_permission_response: RequestPermissionResponse,
    pub session_id: String,
    pub tool_call_id: String,
    #[serde(default)]
    pub save_rule: bool,
}

/// Top-level HTTP body for `/agent/notify-resolved` and `/computer/notify-resolved`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvePermissionHttpRequest {
    pub permission_resolve_request: PermissionResolveRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation_type: Option<String>,
}

impl ResolvePermissionHttpRequest {
    /// Flatten into the internal `ResolvePermissionRequestDto` used by gRPC and the permission
    /// manager.
    pub fn to_dto(&self) -> ResolvePermissionRequestDto {
        let body = &self.permission_resolve_request;
        let (option_id, cancelled) = match &body.request_permission_response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                (Some(selected.option_id.to_string()), false)
            }
            RequestPermissionOutcome::Cancelled => (None, true),
            _ => (None, true),
        };

        ResolvePermissionRequestDto {
            session_id: body.session_id.clone(),
            tool_call_id: body.tool_call_id.clone(),
            option_id,
            cancelled,
            save_rule: body.save_rule,
            project_id: self.project_id.clone(),
            user_id: self.user_id.clone(),
            pod_id: self.pod_id.clone(),
            tenant_id: self.tenant_id.clone(),
            space_id: self.space_id.clone(),
            isolation_type: self.isolation_type.clone(),
        }
    }
}

/// Flat DTO used internally (gRPC, permission manager).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResolvePermissionRequestDto {
    pub session_id: String,
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub save_rule: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation_type: Option<String>,
}

/// Result returned after resolving an ACP permission request.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResolvePermissionResponseDto {
    pub success: bool,
    pub session_id: String,
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_json: Option<String>,
    #[serde(default)]
    pub rule_saved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}
