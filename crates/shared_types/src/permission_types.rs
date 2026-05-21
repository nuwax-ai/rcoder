use agent_client_protocol::schema::{RequestPermissionOutcome, RequestPermissionResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Nested body matching the design doc's `permission_resolve_request` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PermissionResolveRequest {
    /// Full ACP `RequestPermissionResponse` including the tagged outcome.
    #[serde(alias = "requestPermissionResponse")]
    pub request_permission_response: RequestPermissionResponse,
    #[serde(alias = "sessionId")]
    pub session_id: String,
    #[serde(alias = "toolCallId")]
    pub tool_call_id: String,
    #[serde(default, alias = "saveRule")]
    pub save_rule: bool,
}

/// Top-level HTTP body for `/agent/notify-resolved` and `/computer/notify-resolved`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResolvePermissionHttpRequest {
    #[serde(alias = "permissionResolveRequest")]
    pub permission_resolve_request: PermissionResolveRequest,
    #[serde(default, alias = "userId", skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, alias = "projectId", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, alias = "podId", skip_serializing_if = "Option::is_none")]
    pub pod_id: Option<String>,
    #[serde(default, alias = "tenantId", skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    #[serde(default, alias = "spaceId", skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    #[serde(
        default,
        alias = "isolationType",
        skip_serializing_if = "Option::is_none"
    )]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_snake_case_permission_resolution_body() {
        let body: ResolvePermissionHttpRequest = serde_json::from_value(serde_json::json!({
            "permission_resolve_request": {
                "request_permission_response": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": "allow"
                    }
                },
                "session_id": "session_1",
                "tool_call_id": "tool_1",
                "save_rule": true
            },
            "user_id": "user_1",
            "project_id": "project_1"
        }))
        .expect("snake_case body should deserialize");

        let dto = body.to_dto();
        assert_eq!(dto.session_id, "session_1");
        assert_eq!(dto.tool_call_id, "tool_1");
        assert_eq!(dto.option_id.as_deref(), Some("allow"));
        assert!(!dto.cancelled);
        assert!(dto.save_rule);
    }

    #[test]
    fn deserialize_camel_case_permission_resolution_body_for_compatibility() {
        let body: ResolvePermissionHttpRequest = serde_json::from_value(serde_json::json!({
            "permissionResolveRequest": {
                "requestPermissionResponse": {
                    "outcome": {
                        "outcome": "cancelled"
                    }
                },
                "sessionId": "session_2",
                "toolCallId": "tool_2",
                "saveRule": false
            },
            "userId": "user_2",
            "projectId": "project_2",
            "podId": "pod_2",
            "tenantId": "tenant_2",
            "spaceId": "space_2",
            "isolationType": "tenant"
        }))
        .expect("camelCase body should deserialize");

        let dto = body.to_dto();
        assert_eq!(dto.session_id, "session_2");
        assert_eq!(dto.tool_call_id, "tool_2");
        assert!(dto.option_id.is_none());
        assert!(dto.cancelled);
        assert!(!dto.save_rule);
        assert_eq!(dto.user_id.as_deref(), Some("user_2"));
        assert_eq!(dto.project_id.as_deref(), Some("project_2"));
        assert_eq!(dto.pod_id.as_deref(), Some("pod_2"));
        assert_eq!(dto.tenant_id.as_deref(), Some("tenant_2"));
        assert_eq!(dto.space_id.as_deref(), Some("space_2"));
        assert_eq!(dto.isolation_type.as_deref(), Some("tenant"));
    }
}
