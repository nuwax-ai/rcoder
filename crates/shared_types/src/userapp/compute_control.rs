//! Durable compute intent, separate from business-operation slots.
//! Acceptance never proves that an old executor or remote write has stopped.
use super::lifecycle::UserAppOperationScope;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Recorded only after previous executions have drained and compute absence
/// was observed under the physical lease. Recovery must recheck live absence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputeAbsenceCheckpoint {
    pub context: crate::UserAppExecutionContext,
    pub compute_absent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComputeControlAction {
    Stop,
    Restart,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
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
