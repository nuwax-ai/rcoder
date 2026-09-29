//! Early compute handoff contracts. No observation alone authorizes a write.
use crate::{
    AppOperationLease, ComputeControlRecord, ComputeControlState, ServiceType,
    UserAppOperationLeaseReceipt, UserAppOperationScope,
};

/// Admission's image policy is input, not a submitted runtime mutation.
/// Historical null checkpoints remain readable; unknown evidence is preserved.
pub fn compute_unstarted_checkpoint(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Object(fields) => {
            fields.len() == 1
                && fields
                    .get("restart_image_roll")
                    .is_some_and(serde_json::Value::is_boolean)
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
    fn initial_checkpoint_accepts_only_legacy_null_or_image_policy() {
        for value in [
            serde_json::Value::Null,
            serde_json::json!({"restart_image_roll":true}),
            serde_json::json!({"restart_image_roll":false}),
        ] {
            assert!(compute_unstarted_checkpoint(&value));
        }
        for value in [
            serde_json::json!({}),
            serde_json::json!({"restart_image_roll":"true"}),
            serde_json::json!({"restart_image_roll":true,"target":"unknown"}),
        ] {
            assert!(!compute_unstarted_checkpoint(&value));
        }
    }
}
