//! Durable compute intent, separate from business-operation slots.
//! Acceptance never proves that an old executor or remote write has stopped.
use super::lifecycle::UserAppOperationScope;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Request IDs in this namespace are issued only by idle repair admission.
pub const AUTOMATIC_REPAIR_REQUEST_PREFIX: &str = "auto-repair-";

/// Validate public compute controls before discovery or durable admission.
pub fn validate_user_compute_request_id(request_id: &str) -> Result<(), String> {
    crate::validate_identifier(request_id, "request_id")?;
    if request_id.starts_with(AUTOMATIC_REPAIR_REQUEST_PREFIX) {
        return Err("request_id prefix 'auto-repair-' is reserved for internal repair".into());
    }
    Ok(())
}

/// Recorded only after previous executions have drained and compute absence
/// was observed under the physical lease. Recovery must recheck live absence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputeAbsenceCheckpoint {
    pub context: crate::UserAppExecutionContext,
    pub compute_absent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
#[serde(rename_all = "snake_case")]
pub enum ComputeControlAction {
    Stop,
    Restart,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
#[serde(rename_all = "snake_case")]
pub enum ComputeControlState {
    Pending,
    Running,
    RecoveryRequired,
    Succeeded,
    Failed,
    Superseded,
}
impl ComputeControlState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Superseded)
    }

    /// Storage representation for states accepted from an executor progress update.
    pub const fn as_progress_storage_value(self) -> Option<&'static str> {
        match self {
            Self::Running => Some("running"),
            Self::RecoveryRequired => Some("recovery_required"),
            Self::Succeeded => Some("succeeded"),
            Self::Failed => Some("failed"),
            Self::Pending | Self::Superseded => None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ComputeControlRequest {
    pub app_id: String,
    pub lifecycle_id: String,
    pub scope: UserAppOperationScope,
    pub operation_id: String,
    pub request_id: String,
    pub request_fingerprint: String,
    pub action: ComputeControlAction,
    /// A physical ensure uses the restart coordinator to revive a stopped
    /// controller without changing its image. Explicit Restart may roll to
    /// the platform image; the choice is frozen at admission.
    #[serde(default)]
    pub restart_image_roll: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ComputeControlRecord {
    pub app_id: String,
    pub lifecycle_id: String,
    #[serde(skip_serializing)]
    #[schema(ignore)]
    pub request_id: String,
    pub scope: UserAppOperationScope,
    pub operation_id: String,
    #[serde(skip_serializing)]
    #[schema(ignore)]
    pub request_fingerprint: String,
    pub generation: i64,
    pub revision: i64,
    pub action: ComputeControlAction,
    pub state: ComputeControlState,
    pub executor_id: Option<String>,
    pub stage: String,
    pub checkpoint: serde_json::Value,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    /// Private physical receipt; never include it in the public operation view.
    #[serde(skip_serializing)]
    #[schema(ignore)]
    pub lease: Option<super::operation_lease::UserAppOperationLeaseReceipt>,
    /// Captured ordinary operations to drain; these remain in their original slots.
    pub interrupted_operations: Vec<String>,
}

/// Identity attached to every compute mutation and progress commit. A successful
/// check authorizes a caller; it does not fence a request already sent remotely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ComputeExecutorIdentity {
    pub app_id: String,
    pub lifecycle_id: String,
    pub scope: UserAppOperationScope,
    pub operation_id: String,
    pub generation: i64,
    pub executor_id: String,
}

/// A coordinator checkpoint. The store enforces ownership and transition order;
/// the coordinator must validate runtime evidence before submitting it.
#[derive(Debug, Clone)]
pub struct ComputeControlProgress {
    pub identity: ComputeExecutorIdentity,
    pub expected_revision: i64,
    pub state: ComputeControlState,
    pub stage: ComputeControlStage,
    pub checkpoint: serde_json::Value,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
#[serde(rename_all = "snake_case")]
pub enum ComputeControlStage {
    DrainingPrevious,
    Stopping,
    Stopped,
    Starting,
    Verifying,
    Completed,
}
impl ComputeControlStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DrainingPrevious => "draining_previous",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Verifying => "verifying",
            Self::Completed => "completed",
        }
    }

    /// Parse the stage persisted in the control ledger. Unknown values fail closed.
    pub fn from_storage_value(value: &str) -> Option<Self> {
        match value {
            "draining_previous" => Some(Self::DrainingPrevious),
            "stopping" => Some(Self::Stopping),
            "stopped" => Some(Self::Stopped),
            "starting" => Some(Self::Starting),
            "verifying" => Some(Self::Verifying),
            "completed" => Some(Self::Completed),
            _ => None,
        }
    }
}

#[cfg(test)]
mod compute_stage_storage_tests {
    use super::ComputeControlStage as Stage;

    #[test]
    fn stage_storage_vocabulary_round_trips_and_unknown_values_fail_closed() {
        for stage in [
            Stage::DrainingPrevious,
            Stage::Stopping,
            Stage::Stopped,
            Stage::Starting,
            Stage::Verifying,
            Stage::Completed,
        ] {
            assert_eq!(Stage::from_storage_value(stage.as_str()), Some(stage));
        }
        assert_eq!(Stage::from_storage_value("future_stage"), None);
    }
}

/// Whether a diagnostic field is omitted, present but empty, or non-empty.
/// Keeping these cases distinct preserves the executor progress contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum ComputeDiagnosticValue {
    Absent,
    Empty,
    NonEmpty,
}

impl ComputeDiagnosticValue {
    pub fn from_option(value: Option<&str>) -> Self {
        match value {
            None => Self::Absent,
            Some("") => Self::Empty,
            Some(_) => Self::NonEmpty,
        }
    }
}

/// Pure validation failures for one compute-control progress transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeProgressPolicyError {
    InvalidState,
    InvalidStageTransition,
    SuccessRequiresCompletedEvidence,
    CompletedRequiresSuccess,
    FailureRequiresDiagnostics,
    ProgressCannotRetainDiagnostics,
    MutationFailureRequiresRecovery,
    MutationRequiresLeaseAndCheckpoint,
}

impl ComputeProgressPolicyError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::InvalidState => "Invalid compute execution transition",
            Self::InvalidStageTransition => "Invalid compute stage transition",
            Self::SuccessRequiresCompletedEvidence => "Compute success requires completed evidence",
            Self::CompletedRequiresSuccess => "Completed compute stage requires success",
            Self::FailureRequiresDiagnostics => "Compute failure requires structured diagnostics",
            Self::ProgressCannotRetainDiagnostics => {
                "Compute success or progress cannot retain failure diagnostics"
            }
            Self::MutationFailureRequiresRecovery => "Compute mutation failure requires recovery",
            Self::MutationRequiresLeaseAndCheckpoint => {
                "Compute mutation requires a lease and structured checkpoint"
            }
        }
    }
}

/// Validate the finite state/stage/diagnostic policy for an executor update.
///
/// Revision/executor identity, interrupted-operation drain proofs, and runtime
/// receipts remain authoritative checks in the storage transaction. The caller
/// performs its asynchronous drain-evidence read after this pure policy passes.
pub fn validate_compute_progress_transition(
    action: ComputeControlAction,
    current_stage: Option<ComputeControlStage>,
    state: ComputeControlState,
    next_stage: ComputeControlStage,
    error_code: ComputeDiagnosticValue,
    error_message: ComputeDiagnosticValue,
    lease_present: bool,
) -> Result<(), ComputeProgressPolicyError> {
    if state.as_progress_storage_value().is_none() {
        return Err(ComputeProgressPolicyError::InvalidState);
    }

    let legal_stage = current_stage == Some(next_stage)
        || matches!(
            (current_stage, next_stage),
            (
                Some(ComputeControlStage::DrainingPrevious),
                ComputeControlStage::Stopping
            ) | (
                Some(ComputeControlStage::Stopping),
                ComputeControlStage::Stopped
            ) | (
                Some(ComputeControlStage::Stopped),
                ComputeControlStage::Starting
            ) | (
                Some(ComputeControlStage::Starting),
                ComputeControlStage::Verifying
            )
        )
        || (action == ComputeControlAction::Restart
            && current_stage == Some(ComputeControlStage::Verifying)
            && next_stage == ComputeControlStage::Completed)
        || (action == ComputeControlAction::Stop
            && current_stage == Some(ComputeControlStage::Stopped)
            && next_stage == ComputeControlStage::Completed);
    if !legal_stage
        || (action == ComputeControlAction::Stop
            && matches!(
                next_stage,
                ComputeControlStage::Starting | ComputeControlStage::Verifying
            ))
    {
        return Err(ComputeProgressPolicyError::InvalidStageTransition);
    }

    if state == ComputeControlState::Succeeded && next_stage != ComputeControlStage::Completed {
        return Err(ComputeProgressPolicyError::SuccessRequiresCompletedEvidence);
    }
    if next_stage == ComputeControlStage::Completed && state != ComputeControlState::Succeeded {
        return Err(ComputeProgressPolicyError::CompletedRequiresSuccess);
    }
    if matches!(
        state,
        ComputeControlState::Failed | ComputeControlState::RecoveryRequired
    ) && (error_code != ComputeDiagnosticValue::NonEmpty
        || error_message != ComputeDiagnosticValue::NonEmpty)
    {
        return Err(ComputeProgressPolicyError::FailureRequiresDiagnostics);
    }
    if matches!(
        state,
        ComputeControlState::Running | ComputeControlState::Succeeded
    ) && (error_code != ComputeDiagnosticValue::Absent
        || error_message != ComputeDiagnosticValue::Absent)
    {
        return Err(ComputeProgressPolicyError::ProgressCannotRetainDiagnostics);
    }
    // Once physical work started, a failure is uncertain until independently
    // reconciled. Do not let an executor turn a timeout into lease release.
    if state == ComputeControlState::Failed
        && (current_stage != Some(ComputeControlStage::DrainingPrevious) || lease_present)
    {
        return Err(ComputeProgressPolicyError::MutationFailureRequiresRecovery);
    }
    Ok(())
}

/// Validate the non-I/O evidence required after the caller proves old work drained.
pub const fn validate_compute_mutation_evidence(
    stage: ComputeControlStage,
    lease_present: bool,
    checkpoint_is_object: bool,
) -> Result<(), ComputeProgressPolicyError> {
    if !matches!(stage, ComputeControlStage::DrainingPrevious)
        && (!lease_present || !checkpoint_is_object)
    {
        return Err(ComputeProgressPolicyError::MutationRequiresLeaseAndCheckpoint);
    }
    Ok(())
}

#[cfg(kani)]
mod kani_progress_policy_proofs {
    use super::*;

    fn expected_error(
        action: ComputeControlAction,
        current_stage: Option<ComputeControlStage>,
        state: ComputeControlState,
        next_stage: ComputeControlStage,
        error_code: ComputeDiagnosticValue,
        error_message: ComputeDiagnosticValue,
        lease_present: bool,
    ) -> Option<ComputeProgressPolicyError> {
        if !matches!(
            state,
            ComputeControlState::Running
                | ComputeControlState::RecoveryRequired
                | ComputeControlState::Succeeded
                | ComputeControlState::Failed
        ) {
            return Some(ComputeProgressPolicyError::InvalidState);
        }
        let stage_allowed = current_stage == Some(next_stage)
            || matches!(
                (current_stage, next_stage),
                (
                    Some(ComputeControlStage::DrainingPrevious),
                    ComputeControlStage::Stopping
                ) | (
                    Some(ComputeControlStage::Stopping),
                    ComputeControlStage::Stopped
                ) | (
                    Some(ComputeControlStage::Stopped),
                    ComputeControlStage::Starting
                ) | (
                    Some(ComputeControlStage::Starting),
                    ComputeControlStage::Verifying
                )
            )
            || (action == ComputeControlAction::Restart
                && current_stage == Some(ComputeControlStage::Verifying)
                && next_stage == ComputeControlStage::Completed)
            || (action == ComputeControlAction::Stop
                && current_stage == Some(ComputeControlStage::Stopped)
                && next_stage == ComputeControlStage::Completed);
        if !stage_allowed
            || (action == ComputeControlAction::Stop
                && matches!(
                    next_stage,
                    ComputeControlStage::Starting | ComputeControlStage::Verifying
                ))
        {
            return Some(ComputeProgressPolicyError::InvalidStageTransition);
        }
        if state == ComputeControlState::Succeeded && next_stage != ComputeControlStage::Completed {
            return Some(ComputeProgressPolicyError::SuccessRequiresCompletedEvidence);
        }
        if next_stage == ComputeControlStage::Completed && state != ComputeControlState::Succeeded {
            return Some(ComputeProgressPolicyError::CompletedRequiresSuccess);
        }
        if matches!(
            state,
            ComputeControlState::Failed | ComputeControlState::RecoveryRequired
        ) && (error_code != ComputeDiagnosticValue::NonEmpty
            || error_message != ComputeDiagnosticValue::NonEmpty)
        {
            return Some(ComputeProgressPolicyError::FailureRequiresDiagnostics);
        }
        if matches!(
            state,
            ComputeControlState::Running | ComputeControlState::Succeeded
        ) && (error_code != ComputeDiagnosticValue::Absent
            || error_message != ComputeDiagnosticValue::Absent)
        {
            return Some(ComputeProgressPolicyError::ProgressCannotRetainDiagnostics);
        }
        if state == ComputeControlState::Failed
            && (current_stage != Some(ComputeControlStage::DrainingPrevious) || lease_present)
        {
            return Some(ComputeProgressPolicyError::MutationFailureRequiresRecovery);
        }
        None
    }

    /// Exhaustively checks the production policy over every state/action/stage
    /// combination, including persisted-stage corruption and diagnostic presence.
    #[kani::proof]
    fn compute_progress_policy_matches_complete_finite_contract() {
        let action: ComputeControlAction = kani::any();
        let current_stage: Option<ComputeControlStage> = kani::any();
        let state: ComputeControlState = kani::any();
        let next_stage: ComputeControlStage = kani::any();
        let error_code: ComputeDiagnosticValue = kani::any();
        let error_message: ComputeDiagnosticValue = kani::any();
        let lease_present: bool = kani::any();

        let actual = validate_compute_progress_transition(
            action,
            current_stage,
            state,
            next_stage,
            error_code,
            error_message,
            lease_present,
        );
        // A Stop intent has no start/verification phase. Check both the
        // persisted and requested stages directly so this safety rule cannot
        // silently drift with the reference contract below.
        if action == ComputeControlAction::Stop
            && (matches!(
                current_stage,
                Some(ComputeControlStage::Starting | ComputeControlStage::Verifying)
            ) || matches!(
                next_stage,
                ComputeControlStage::Starting | ComputeControlStage::Verifying
            ))
        {
            assert!(actual.is_err());
        }
        assert_eq!(
            actual.err(),
            expected_error(
                action,
                current_stage,
                state,
                next_stage,
                error_code,
                error_message,
                lease_present,
            )
        );

        let checkpoint_is_object: bool = kani::any();
        let evidence_actual =
            validate_compute_mutation_evidence(next_stage, lease_present, checkpoint_is_object);
        let evidence_required = next_stage == ComputeControlStage::DrainingPrevious
            || (lease_present && checkpoint_is_object);
        assert_eq!(evidence_actual.is_ok(), evidence_required);
    }
}

#[cfg(test)]
mod compute_progress_policy_tests {
    use super::*;

    #[test]
    fn stop_completion_requires_stopped_while_restart_completes_after_verifying() {
        let stop_from_verifying = validate_compute_progress_transition(
            ComputeControlAction::Stop,
            Some(ComputeControlStage::Verifying),
            ComputeControlState::Succeeded,
            ComputeControlStage::Completed,
            ComputeDiagnosticValue::Absent,
            ComputeDiagnosticValue::Absent,
            true,
        );
        assert_eq!(
            stop_from_verifying,
            Err(ComputeProgressPolicyError::InvalidStageTransition)
        );

        let restart_from_verifying = validate_compute_progress_transition(
            ComputeControlAction::Restart,
            Some(ComputeControlStage::Verifying),
            ComputeControlState::Succeeded,
            ComputeControlStage::Completed,
            ComputeDiagnosticValue::Absent,
            ComputeDiagnosticValue::Absent,
            true,
        );
        assert!(restart_from_verifying.is_ok());

        let stop_from_stopped = validate_compute_progress_transition(
            ComputeControlAction::Stop,
            Some(ComputeControlStage::Stopped),
            ComputeControlState::Succeeded,
            ComputeControlStage::Completed,
            ComputeDiagnosticValue::Absent,
            ComputeDiagnosticValue::Absent,
            true,
        );
        assert!(stop_from_stopped.is_ok());
    }
}

/// Internal executor acknowledgement, never accepted directly from an HTTP
/// caller. Executor acknowledgements require definite runtime results; recovery
/// observers may only use a persisted mutation boundary validated by the store.
#[derive(Debug, Clone)]
pub struct ComputeControlDrainAcknowledgement {
    pub identity: ComputeExecutorIdentity,
    pub expected_revision: i64,
    pub evidence: ComputeControlDrainEvidence,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ComputeControlDrainEvidence {
    NoMutationSubmitted,
    /// Original API call completed and its exact runtime target was durably
    /// recorded. The successor still has to perform its own Stop.
    PersistedRuntimeAcknowledgement {
        checkpoint: serde_json::Value,
    },
    /// Runtime consumed or observed the loss of the original single-write
    /// precondition. This says nothing about business readiness or stop success.
    ConditionalStartupFenced {
        checkpoint: serde_json::Value,
    },
    /// Original scale-down precondition is consumed; its Pod exit wait may
    /// still run, but cannot submit a subsequent startup after supersession.
    ConditionalStopFenced {
        checkpoint: serde_json::Value,
    },
    /// An observer found a persisted post-write boundary. The exact revision,
    /// stage and checkpoint must still match when the store closes the record.
    PersistedMutationBoundary {
        checkpoint: serde_json::Value,
    },
    /// Same-operation runtime acknowledgement, validated by its runtime adapter.
    MutationCompleted {
        checkpoint: serde_json::Value,
    },
}

impl ComputeControlRecord {
    pub fn has_docker_compute_target(&self) -> bool {
        let Ok(context) = self.execution_context() else {
            return false;
        };
        match self.scope {
            UserAppOperationScope::Dev => {
                serde_json::from_value::<crate::BuilderControlTarget>(self.checkpoint.clone())
                    .is_ok_and(|target| {
                        target.context == context
                            && target.validate().is_ok()
                            && target.workload.is_some_and(|workload| {
                                workload.kind == crate::AppResourceKind::Container
                            })
                    })
            }
            UserAppOperationScope::Prod => {
                serde_json::from_value::<crate::UserAppMutationTarget>(self.checkpoint.clone())
                    .is_ok_and(|target| {
                        target.context == context
                            && target.resource.kind == crate::AppResourceKind::Container
                            && !target.resource.uid.is_empty()
                            && !target.resource.name.is_empty()
                    })
            }
            UserAppOperationScope::Application => false,
        }
    }
    pub fn has_conditional_compute_write(&self) -> bool {
        let field = match self.scope {
            UserAppOperationScope::Dev => "builder_compute_single_write",
            UserAppOperationScope::Prod => "compute_start_single_write",
            UserAppOperationScope::Application => return false,
        };
        self.checkpoint
            .get(field)
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    }
    pub fn execution_context(&self) -> Result<crate::UserAppExecutionContext, String> {
        let context = crate::UserAppExecutionContext {
            app_id: self.app_id.clone(),
            lifecycle_id: self.lifecycle_id.clone(),
            operation_id: self.operation_id.clone(),
            executor_id: self
                .executor_id
                .clone()
                .ok_or("Compute executor is not claimed")?,
            request_fingerprint: self.request_fingerprint.clone(),
        };
        context.validate_identity(&self.app_id)?;
        Ok(context)
    }
}
