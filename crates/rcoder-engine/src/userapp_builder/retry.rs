//! Explicit retry delegates to the same Pending kernels as restart discovery.
use crate::app_state::AppState;
use shared_types::{UserAppOperationKind as Kind, UserAppOperationRecord, UserAppOperationState};
use std::sync::Weak;

pub(crate) struct PendingBuilderRecovery {
    state: Weak<AppState>,
}
impl PendingBuilderRecovery {
    pub(crate) fn new(state: Weak<AppState>) -> Self {
        Self { state }
    }
}
#[async_trait::async_trait]
impl shared_types::UserAppBuilderRecovery for PendingBuilderRecovery {
    async fn resume_pending(&self, operation: &UserAppOperationRecord) -> Result<bool, String> {
        if operation.state != UserAppOperationState::Pending {
            return Err("Builder retry requires an unclaimed operation".into());
        }
        let state = self
            .state
            .upgrade()
            .ok_or("Application state is unavailable")?;
        let result = match operation.kind {
            Kind::EnsureBuilder => super::creation::resume_pending(&state, operation).await,
            Kind::StopBuilder | Kind::RestartBuilder => {
                super::control::resume_pending(&state, operation).await
            }
            Kind::AdoptBuilder => super::adoption::resume_pending(&state, operation).await,
            Kind::AdoptApplication => super::app_adoption::resume_pending(&state, operation).await,
            _ => return Err("Operation is not a Pending builder command".into()),
        };
        result.map_err(|error| format!("Resume original builder operation: {error:#}"))
    }
    async fn reconcile_completed(
        &self,
        operation: &UserAppOperationRecord,
    ) -> Result<bool, String> {
        if !matches!(
            operation.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        ) || !shared_types::userapp_operation_has_final_evidence(operation)
        {
            return Err("Builder completion recovery requires confirmed final evidence".into());
        }
        let state = self
            .state
            .upgrade()
            .ok_or("Application state is unavailable")?;
        let result = match operation.kind {
            Kind::EnsureBuilder => super::creation::reconcile_completed(&state, operation).await,
            Kind::StopBuilder | Kind::RestartBuilder => {
                super::control::reconcile_completed(&state, operation).await
            }
            _ => return Err("Operation has no builder completion reconciler".into()),
        };
        result.map_err(|error| format!("Reconcile original builder operation: {error:#}"))
    }
}
