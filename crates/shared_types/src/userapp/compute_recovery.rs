//! Early compute handoff contracts. No observation alone authorizes a write.
use crate::{
    AppOperationLease, ComputeControlRecord, ComputeControlState, ServiceType,
    UserAppOperationLeaseReceipt, UserAppOperationScope,
};

/// Admission's image/startup policies are inputs, not submitted runtime writes.
/// Historical null checkpoints remain readable; unknown evidence is preserved.
pub fn compute_unstarted_checkpoint(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Object(fields) => {
            fields
                .keys()
                .all(|key| matches!(key.as_str(), "restart_image_roll" | "builder_startup_mode"))
                && fields
                    .get("restart_image_roll")
                    .is_some_and(serde_json::Value::is_boolean)
                && fields.get("builder_startup_mode").is_none_or(|mode| {
                    serde_json::from_value::<crate::BuilderStartupMode>(mode.clone()).is_ok()
                })
        }
        _ => false,
    }
}

impl ComputeControlRecord {
    pub fn can_recover_compute_drain(&self) -> bool {
        matches!(
            self.state,
            ComputeControlState::Running | ComputeControlState::RecoveryRequired
        ) && self.stage == "draining_previous"
            && self.executor_id.is_some()
            && compute_unstarted_checkpoint(&self.checkpoint)
    }
}

pub fn compute_lease_family(scope: UserAppOperationScope) -> Result<ServiceType, String> {
    match scope {
        UserAppOperationScope::Dev => Ok(ServiceType::UserappBuilder),
        UserAppOperationScope::Prod => Ok(ServiceType::Userapp),
        UserAppOperationScope::Application => {
            Err("Compute lease requires dev or prod scope".into())
        }
    }
}

/// Prepared leases exclude other writers. Activate only AFTER durable binding.
/// Docker preparation deliberately leaves the marker empty until activation.
#[async_trait::async_trait]
pub trait PreparedComputeLease: AppOperationLease {
    async fn activate(&mut self) -> Result<(), String>;
}

/// Used only after a durable early-stage handoff has revoked the old executor.
/// Releasable does not claim that arbitrary remote writes have finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputeLeaseInspection {
    Absent,
    Releasable(UserAppOperationLeaseReceipt),
    /// An earlier prepared attempt can arrive after a lost acquire response.
    /// Its complete runtime metadata must match the SAME operation/input.
    Discovered {
        receipt: UserAppOperationLeaseReceipt,
        attempt: crate::UserAppExecutionContext,
    },
    Held,
    IdentityChanged(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initial_checkpoint_accepts_only_known_unstarted_policy_inputs() {
        for value in [
            serde_json::Value::Null,
            serde_json::json!({"restart_image_roll":true}),
            serde_json::json!({"restart_image_roll":false}),
            serde_json::json!({"restart_image_roll":false,"builder_startup_mode":"management_only"}),
            serde_json::json!({"restart_image_roll":true,"builder_startup_mode":"preserve_business_intent"}),
        ] {
            assert!(compute_unstarted_checkpoint(&value));
        }
        for value in [
            serde_json::json!({}),
            serde_json::json!({"restart_image_roll":"true"}),
            serde_json::json!({"restart_image_roll":true,"target":"unknown"}),
            serde_json::json!({"restart_image_roll":true,"builder_startup_mode":"unknown"}),
            serde_json::json!({"restart_image_roll":true,"builder_startup_mode":"management_only","unknown_write":true}),
        ] {
            assert!(!compute_unstarted_checkpoint(&value));
        }
    }
}
