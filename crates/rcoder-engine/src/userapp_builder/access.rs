//! Why a request needs the development container. Compute and business intent
//! stay separate: file access may revive management without replaying business.
use super::{BuilderComputeState, compute_control, inspect_builder_compute};
use crate::app_state::AppState;
use anyhow::{Result, anyhow, ensure};
use shared_types::{
    BuilderStartupMode, ComputeControlAction, ComputeControlState, UserAppControlRequest,
    UserAppLifecycleRecord, UserAppLifecycleState, UserAppOperationScope, UserAppWaitTimeout,
};
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuilderAccessIntent {
    Management,
    SourceExecution,
}

impl BuilderAccessIntent {
    pub(crate) fn startup_mode(self) -> BuilderStartupMode {
        // The original Source request is delivered once after management is
        // reachable. Compute recovery must never run its business implicitly.
        match self {
            Self::Management | Self::SourceExecution => BuilderStartupMode::ManagementOnly,
        }
    }
}

/// Only a confirmed completed Stop may enter the existing removed-container
/// creation path which CAS-clears that Stop intent. Other callers cannot clear it.
pub(super) async fn prepare(
    state: &AppState,
    app: &UserAppLifecycleRecord,
    intent: BuilderAccessIntent,
    deadline: Instant,
) -> Result<bool> {
    let scope = UserAppOperationScope::Dev;
    let status = state
        .userapp_store
        .read_compute_status(&app.app_id, &app.lifecycle_id, scope)
        .await?;
    if !status.desired_stopped {
        // An external container stop has no platform Stop record. Restore its
        // management intent through the same durable compute flow, rather than
        // allowing ordinary ensure to start an old business environment.
        if app.active_operations.dev.is_none()
            && app.active_operations.application.is_none()
            && status
                .operation
                .as_ref()
                .is_none_or(|operation| operation.state.is_terminal())
            && retained_compute_is_actually_stopped(state, app).await?
        {
            wake_retained(state, app, intent, deadline).await?;
        }
        return Ok(false);
    }
    // This preserves a real in-flight Stop/Restart instead of erasing its
    // intent because a file or source request arrived later.
    if status
        .operation
        .as_ref()
        .is_some_and(|operation| !operation.state.is_terminal())
    {
        state
            .userapp_store
            .check_compute_access(&app.app_id, scope, false)
            .await?;
        return Err(anyhow!(
            "An active compute stop was not returned as a conflict"
        ));
    }
    let stop = status
        .operation
        .ok_or_else(|| anyhow!("Stopped compute intent has no control operation"))?;
    ensure!(
        stop.lifecycle_id == app.lifecycle_id
            && stop.scope == scope
            && stop.action == ComputeControlAction::Stop
            && stop.state == ComputeControlState::Succeeded,
        "Previous compute stop has not been confirmed"
    );
    wait_original_release(state, &stop, deadline).await?;
    match inspect_builder_compute(state, &app.app_id).await? {
        BuilderComputeState::StoppedRemoved => Ok(true),
        BuilderComputeState::StoppedRetained => {
            wake_retained(state, app, intent, deadline).await?;
            Ok(false)
        }
        BuilderComputeState::Missing | BuilderComputeState::Running(_) => Err(anyhow!(
            "Stopped builder observation changed before management recovery; retry access"
        )),
    }
}

async fn retained_compute_is_actually_stopped(
    state: &AppState,
    app: &UserAppLifecycleRecord,
) -> Result<bool> {
    let context = shared_types::UserAppExecutionContext {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        operation_id: "management-stopped-observation".into(),
        executor_id: "reader".into(),
        request_fingerprint: "0".repeat(64),
    };
    let target = super::adoption::capture_bound_target(state, &context).await?;
    if target.workload.is_none() {
        return Ok(false);
    }
    state
        .runtime()
        .observe_builder_compute_stopped(&target)
        .await
        .map_err(Into::into)
}

async fn wake_retained(
    state: &AppState,
    app: &UserAppLifecycleRecord,
    intent: BuilderAccessIntent,
    deadline: Instant,
) -> Result<()> {
    let operation = compute_control::submit_physical_start_for_access(
        &Arc::new(state.clone()),
        app.app_id.clone(),
        UserAppOperationScope::Dev,
        UserAppControlRequest {
            lifecycle_id: Some(app.lifecycle_id.clone()),
            request_id: None,
        },
        intent.startup_mode(),
    )
    .await?;
    wait_started(state, app, &operation.operation_id, deadline).await
}

async fn wait_original_release(
    state: &AppState,
    stop: &shared_types::ComputeControlRecord,
    deadline: Instant,
) -> Result<()> {
    tokio::time::timeout_at(deadline, wait_original_release_inner(state, stop, deadline))
        .await
        .map_err(|_| UserAppWaitTimeout {
            operation_id: Some(stop.operation_id.clone()),
        })?
}

async fn wait_original_release_inner(
    state: &AppState,
    stop: &shared_types::ComputeControlRecord,
    deadline: Instant,
) -> Result<()> {
    loop {
        check_deadline(deadline, &stop.operation_id)?;
        let current = state
            .userapp_store
            .get_compute_control(&stop.app_id, &stop.operation_id)
            .await?
            .ok_or_else(|| {
                anyhow!("Original compute stop disappeared during release observation")
            })?;
        ensure!(
            current.lifecycle_id == stop.lifecycle_id
                && current.generation == stop.generation
                && current.action == stop.action
                && current.state == ComputeControlState::Succeeded,
            "Original compute stop changed during release observation"
        );
        if current.lease.is_none() {
            return Ok(());
        }
        wait_round(deadline, &stop.operation_id).await?;
    }
}

async fn wait_started(
    state: &AppState,
    app: &UserAppLifecycleRecord,
    operation_id: &str,
    deadline: Instant,
) -> Result<()> {
    tokio::time::timeout_at(
        deadline,
        wait_started_inner(state, app, operation_id, deadline),
    )
    .await
    .map_err(|_| UserAppWaitTimeout {
        operation_id: Some(operation_id.to_owned()),
    })?
}

async fn wait_started_inner(
    state: &AppState,
    app: &UserAppLifecycleRecord,
    operation_id: &str,
    deadline: Instant,
) -> Result<()> {
    loop {
        check_deadline(deadline, operation_id)?;
        let current = state
            .userapp_store
            .get_compute_control(&app.app_id, operation_id)
            .await?
            .ok_or_else(|| anyhow!("Management recovery operation disappeared"))?;
        let lifecycle = state
            .userapp_store
            .get_application(&app.app_id)
            .await?
            .ok_or_else(|| anyhow!("Application disappeared during management recovery"))?;
        ensure!(
            current.lifecycle_id == app.lifecycle_id
                && current.scope == UserAppOperationScope::Dev
                && lifecycle.lifecycle_id == app.lifecycle_id
                && lifecycle.state == UserAppLifecycleState::Active,
            "Management recovery changed application lifecycle"
        );
        match current.state {
            ComputeControlState::Succeeded if current.lease.is_none() => return Ok(()),
            ComputeControlState::Failed | ComputeControlState::Superseded => {
                return Err(shared_types::BuilderControlError {
                    operation_id: operation_id.into(),
                    message: current.error_message.unwrap_or_else(|| {
                        format!("Management recovery ended as {:?}", current.state)
                    }),
                }
                .into());
            }
            _ => wait_round(deadline, operation_id).await?,
        }
    }
}

async fn wait_round(deadline: Instant, operation_id: &str) -> Result<()> {
    check_deadline(deadline, operation_id)?;
    tokio::time::sleep_until((Instant::now() + Duration::from_millis(100)).min(deadline)).await;
    Ok(())
}

fn check_deadline(deadline: Instant, operation_id: &str) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(UserAppWaitTimeout {
            operation_id: Some(operation_id.into()),
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
