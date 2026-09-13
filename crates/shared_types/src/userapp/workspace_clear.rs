//! Internal file-server workspace reset protocol. Instance identity fences a
//! process replacement between target observation and mutation submission.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// An uncached runtime observation, bound to the captured workload and lifecycle.
/// The address must select one container/pod, never a load-balanced Service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAppBuilderWorkspaceEndpoint {
    /// Actual Docker container ID or Kubernetes Pod UID from the captured runtime.
    pub container_id: String,
    /// Direct address of that container or Pod, not a Service or load balancer.
    pub address: std::net::IpAddr,
}

impl UserAppBuilderWorkspaceEndpoint {
    pub fn base_url(&self) -> String {
        format!(
            "http://{}",
            std::net::SocketAddr::new(self.address, crate::AGENT_FILE_SERVER_PORT)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserAppWorkspaceClearProbe {
    /// Business application identifier whose development workspace is selected.
    pub app_id: String,
    /// Application owner used to verify access and select the owner workspace.
    pub user_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserAppWorkspaceClearTarget {
    /// Business application identifier whose development workspace is selected.
    pub app_id: String,
    /// Nonempty nonce identifying this file-server process instance. It changes
    /// on restart and is distinct from the application lifecycle and operation ID.
    pub instance_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserAppWorkspaceClearRequest {
    /// Business application identifier whose development workspace is selected.
    pub app_id: String,
    /// Application owner used to verify access and select the owner workspace.
    pub user_id: String,
    /// Exact process nonce returned by the target probe. A mismatch rejects the
    /// reset before stopping workers or clearing files; this is not a lifecycle ID.
    pub expected_instance_id: String,
}

/// Successful reset acknowledgement. Required fields deliberately have no
/// defaults: an HTTP 200 error envelope or an older response is not confirmation.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserAppWorkspaceClearResult {
    /// True only after the requested workspace reset has completed. False or a
    /// missing field must not be interpreted as a successful acknowledgement.
    pub success: bool,
    /// Nonempty nonce identifying this file-server process instance. It changes
    /// on restart and is distinct from the application lifecycle and operation ID.
    pub instance_id: String,
}

impl UserAppWorkspaceClearResult {
    pub fn confirms(&self, expected_instance_id: &str) -> bool {
        self.success && !expected_instance_id.is_empty() && self.instance_id == expected_instance_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_confirmation_requires_success_and_the_observed_instance() {
        for (success, instance, expected, confirmed) in [
            (true, "current", "current", true),
            (false, "current", "current", false),
            (true, "previous", "current", false),
            (true, "", "", false),
        ] {
            let response = UserAppWorkspaceClearResult {
                success,
                instance_id: instance.into(),
            };
            assert_eq!(response.confirms(expected), confirmed);
        }
        for value in [
            serde_json::json!({"success": true}),
            serde_json::json!({"instance_id": "current"}),
            serde_json::json!({"success": false, "code": "ERR_BACKEND_ERROR"}),
            serde_json::json!({"success": true, "data": {"instance_id": "current"}}),
        ] {
            assert!(serde_json::from_value::<UserAppWorkspaceClearResult>(value).is_err());
        }
    }
}
