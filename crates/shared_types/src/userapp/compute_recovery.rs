//! Early compute handoff contracts. No observation alone authorizes a write.
use crate::{
    AppOperationLease, ComputeControlRecord, ComputeControlState, ServiceType,
    UserAppOperationLeaseReceipt, UserAppOperationScope,
};

/// Internal controller boundary. It is not an HTTP or persistent schema: the
/// existing stage CAS decides when stop/start calls may be submitted, while
/// runtime mutex observation separately proves whether an executor still runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EarlyComputeBoundary {
    BeforeContainerSubmission,
    OutcomeUnconfirmed,
    OutsideEarlyPhase,
}

/// Explicit compatibility reader for the original early-checkpoint format.
/// Policy values and historical diagnostics are not stop/start receipts. The
/// supported legacy unknown-outcome flag remains meaningful; it is not inferred
/// from arbitrary JSON names or from the presence of an old captured target.
#[derive(serde::Deserialize)]
struct LegacyEarlyCheckpoint {
    #[serde(default)]
    unknown_write: Option<bool>,
}

impl LegacyEarlyCheckpoint {
    fn boundary(value: &serde_json::Value) -> EarlyComputeBoundary {
        if value.is_null() {
            return EarlyComputeBoundary::BeforeContainerSubmission;
        }
        match serde_json::from_value::<Self>(value.clone()) {
            Ok(Self {
                unknown_write: Some(true),
            })
            | Err(_) => EarlyComputeBoundary::OutcomeUnconfirmed,
            Ok(_) => EarlyComputeBoundary::BeforeContainerSubmission,
        }
    }
}

/// Compatibility with callers that inspect an admitted early checkpoint. This
/// result never authorizes a write: record eligibility and actual mutex facts
/// remain mandatory, and a later persisted controller phase cannot use it.
pub fn compute_unstarted_checkpoint(value: &serde_json::Value) -> bool {
    LegacyEarlyCheckpoint::boundary(value) == EarlyComputeBoundary::BeforeContainerSubmission
}

impl ComputeControlRecord {
    pub fn early_compute_boundary(&self) -> EarlyComputeBoundary {
        if crate::ComputeControlStage::from_storage_value(&self.stage)
            != Some(crate::ComputeControlStage::DrainingPrevious)
            || self.executor_id.is_none()
        {
            return EarlyComputeBoundary::OutsideEarlyPhase;
        }
        LegacyEarlyCheckpoint::boundary(&self.checkpoint)
    }

    /// Supersession revoked all attempts before a runtime stop/start boundary.
    /// Policy, diagnostic and old archive fields do not authorize a mutation.
    pub fn can_observe_superseded_compute_drain(&self) -> bool {
        self.state == ComputeControlState::Superseded
            && self.action == crate::ComputeControlAction::Restart
            && self.early_compute_boundary() == EarlyComputeBoundary::BeforeContainerSubmission
    }

    /// An original NoMutationSubmitted ACK keeps its executor lineage, allowing
    /// the issued successor to observe a prepared lease that arrives late.
    pub fn has_unsubmitted_compute_drain_ack(&self) -> bool {
        self.state == ComputeControlState::Failed
            && self.stage == "superseded_drained"
            && self.error_code.as_deref() == Some("ERR_CANCELLED")
            && self.executor_id.is_some()
            && self.checkpoint.get("drain").is_some_and(|value| {
                matches!(
                    serde_json::from_value::<crate::ComputeControlDrainEvidence>(value.clone()),
                    Ok(crate::ComputeControlDrainEvidence::NoMutationSubmitted)
                )
            })
            && self.checkpoint.get("previous").is_some_and(|value| {
                LegacyEarlyCheckpoint::boundary(value)
                    == EarlyComputeBoundary::BeforeContainerSubmission
            })
    }

    pub fn can_recover_compute_drain(&self) -> bool {
        matches!(
            self.state,
            ComputeControlState::Running | ComputeControlState::RecoveryRequired
        ) && self.early_compute_boundary() == EarlyComputeBoundary::BeforeContainerSubmission
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
    /// An expired ordinary operation mutex remains after its durable receipt
    /// was forgotten. This read-only candidate grants no cleanup or new write:
    /// the coordinator must prove that this exact owner is terminal and fenced,
    /// then conditionally release only the unchanged observed mutex.
    PreviousOwner {
        receipt: UserAppOperationLeaseReceipt,
        owner: crate::UserAppExecutionContext,
    },
    /// A free Docker mutex records an ordinary operation ID, not a compute
    /// executor token. Resolve its old owner from the control store before
    /// conditional cleanup; the marker alone grants no execution authority.
    PreviousOperation {
        receipt: UserAppOperationLeaseReceipt,
        operation_id: String,
    },
    Held,
    IdentityChanged(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn early_compatibility_uses_original_outcome_and_not_json_field_names() {
        for value in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({"diagnostic":{"old_epoch":3},"builder_restart_template":"old opaque archive","builder_startup_mode":"old-policy"}),
            // A diagnostic extension using a former blacklist name is not an
            // acknowledged write. Eligibility still requires the actual stage.
            serde_json::json!({"target":"old diagnostic","context":"old context note","workload":"history"}),
        ] {
            assert!(compute_unstarted_checkpoint(&value));
        }
        for value in [
            serde_json::json!({"unknown_write":true}),
            serde_json::json!({"unknown_write":"unknown old outcome"}),
            serde_json::Value::String("unreadable original outcome".into()),
        ] {
            assert!(!compute_unstarted_checkpoint(&value));
        }
    }
    #[test]
    fn initial_checkpoint_retains_policy_and_diagnostics_without_changing_current_outcome() {
        for value in [
            serde_json::Value::Null,
            serde_json::json!({"restart_image_roll":true}),
            serde_json::json!({"restart_image_roll":false,"builder_startup_mode":"management_only"}),
            serde_json::json!({"restart_image_roll":true,"builder_startup_mode":"old-policy","diagnostic":{"phase":"Running"}}),
        ] {
            assert!(compute_unstarted_checkpoint(&value));
        }
        assert!(!compute_unstarted_checkpoint(
            &serde_json::json!({"restart_image_roll":true,"unknown_write":true})
        ));
    }
    #[test]
    fn persistent_stage_not_checkpoint_field_names_defines_submission_boundary() {
        let mut record = ComputeControlRecord {
            created_at: chrono::Utc::now(),
            app_id: "fixture-app".into(),
            lifecycle_id: "fixture-life".into(),
            request_id: "original-request".into(),
            scope: UserAppOperationScope::Dev,
            operation_id: "original-operation".into(),
            request_fingerprint: "original-input".into(),
            generation: 1,
            revision: 1,
            action: crate::ComputeControlAction::Restart,
            state: ComputeControlState::Superseded,
            executor_id: Some("revoked-executor".into()),
            stage: crate::ComputeControlStage::DrainingPrevious.as_str().into(),
            checkpoint: serde_json::json!({"target":{"workload":"historical diagnostic"},"new_diagnostic":3}),
            error_code: None,
            error_message: None,
            lease: None,
            interrupted_operations: vec![],
        };
        assert!(record.can_observe_superseded_compute_drain());
        for stage in [
            crate::ComputeControlStage::Stopping,
            crate::ComputeControlStage::Stopped,
            crate::ComputeControlStage::Starting,
            crate::ComputeControlStage::Verifying,
            crate::ComputeControlStage::Completed,
        ] {
            record.stage = stage.as_str().into();
            record.checkpoint = serde_json::Value::Null;
            assert_eq!(
                record.early_compute_boundary(),
                EarlyComputeBoundary::OutsideEarlyPhase
            );
            assert!(!record.can_observe_superseded_compute_drain());
        }
        record.stage = crate::ComputeControlStage::DrainingPrevious.as_str().into();
        record.checkpoint = serde_json::json!({"unknown_write":true});
        assert_eq!(
            record.early_compute_boundary(),
            EarlyComputeBoundary::OutcomeUnconfirmed
        );
        assert!(!record.can_observe_superseded_compute_drain());
    }
}
