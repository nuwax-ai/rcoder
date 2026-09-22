//! Detached compute coordination. Admission never waits for a business slot.
//! Runtime calls retain their physical receipt until their outcome is confirmed.
use super::dev_cleanup::BuilderOperation;
use crate::app_state::AppState;
use anyhow::{Context, Result, anyhow, ensure};
use futures::FutureExt as _;
use sha2::{Digest, Sha256};
use shared_types::*;
use std::{sync::Arc, time::Duration};

#[derive(serde::Serialize, utoipa::ToSchema)]
pub struct ComputeOperationView {
    pub operation_id: String,
    pub app_id: String,
    pub lifecycle_id: String,
    /// Operation slot scope: Dev / Prod / Application.
    pub scope: UserAppOperationScope,
    /// Compute intent kind: one of stop / restart.
    pub action: ComputeControlAction,
    /// Intent state: pending / running / recovery_required / succeeded /
    /// failed / superseded.
    pub state: ComputeControlState,
    pub stage: String,
    pub revision: i64,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub status_url: String,
}
impl From<ComputeControlRecord> for ComputeOperationView {
    fn from(r: ComputeControlRecord) -> Self {
        Self {
            status_url: format!("/computer/pod/operations/{}/{}", r.app_id, r.operation_id),
            operation_id: r.operation_id,
            app_id: r.app_id,
            lifecycle_id: r.lifecycle_id,
            scope: r.scope,
            action: r.action,
            state: r.state,
            stage: r.stage,
            revision: r.revision,
            error_code: r.error_code,
            error_message: r.error_message,
        }
    }
}

pub async fn submit(
    state: &Arc<AppState>,
    app_id: String,
    scope: UserAppOperationScope,
    action: ComputeControlAction,
    request: UserAppControlRequest,
) -> Result<ComputeOperationView> {
    validate_identifier(&app_id, "app_id").map_err(anyhow::Error::msg)?;
    let app = super::adoption::discover_missing_identity(state, &app_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if app.state != UserAppLifecycleState::Active {
        return Err(UserAppStoreError::LifecycleConflict.into());
    }
    if request
        .lifecycle_id
        .as_ref()
        .is_some_and(|id| id != &app.lifecycle_id)
    {
        return Err(UserAppStoreError::LifecycleConflict.into());
    }
    let request_id = request
        .request_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_identifier(&request_id, "request_id").map_err(anyhow::Error::msg)?;
    let fingerprint = Sha256::digest(serde_json::to_vec(&(
        &app_id,
        &app.lifecycle_id,
        scope,
        action,
    ))?)
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
    // Own shutdown accounting before admission; HTTP cancellation cannot discard
    // a committed intent. Pending records are also picked up by discovery.
    let flight = state.userapp_op_flight.guard()?;
    let record = state
        .userapp_store
        .admit_compute_control(&ComputeControlRequest {
            app_id,
            lifecycle_id: app.lifecycle_id,
            scope,
            action,
            request_id,
            operation_id: uuid::Uuid::new_v4().to_string(),
            request_fingerprint: fingerprint,
        })
        .await?;
    if record.state == ComputeControlState::Pending {
        let state = state.clone();
        let pending = record.clone();
        tokio::spawn(async move {
            let _flight = flight;
            if let Err(error) = execute_pending(&state, pending).await {
                tracing::error!(%error, "Compute coordinator stopped; inspect durable operation");
            }
        });
    }
    Ok(record.into())
}

/// An explicit retry keeps the operation/request identity and cannot discard
/// unknown runtime effects. Pending discovery closes the HTTP/spawn crash gap.
pub async fn recover(
    state: &Arc<AppState>,
    app_id: &str,
    operation_id: &str,
    expected_revision: i64,
) -> Result<ComputeOperationView> {
    let current = state
        .userapp_store
        .get_compute_control(app_id, operation_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if current.revision != expected_revision {
        return Err(UserAppStoreError::VersionConflict.into());
    }
    let flight = state.userapp_op_flight.guard()?;
    if current.action == ComputeControlAction::Restart
        && (current.stage == "stopped"
            || (current.stage == "starting" && current.has_conditional_compute_write()))
    {
        let worker = current.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let _flight = flight;
            if let Err(error) = resume_restart_start(&state, &worker).await {
                tracing::warn!(%error, "Restart continuation requires inspection");
            }
        });
        return Ok(current.into());
    }
    if matches!(
        (current.action, current.stage.as_str()),
        (ComputeControlAction::Stop, "stopped" | "stopping")
            | (ComputeControlAction::Restart, "verifying" | "starting")
    ) || (current.scope == UserAppOperationScope::Prod
        && matches!(
            (current.action, current.stage.as_str()),
            (ComputeControlAction::Stop, "stopping") | (ComputeControlAction::Restart, "starting")
        ))
    {
        let completed = recover_confirmed(state, &current).await?;
        drop(flight);
        return Ok(completed.into());
    }
    let pending = state.userapp_store.resume_compute_drain(&current).await?;
    let worker = pending.clone();
    let state = state.clone();
    tokio::spawn(async move {
        let _flight = flight;
        if let Err(error) = execute_pending(&state, worker).await {
            tracing::error!(%error, "Compute recovery requires further inspection");
        }
    });
    Ok(pending.into())
}

/// Recover a durable completion boundary or an exact atomic remote stop receipt.
/// This path contains no start/stop/redeploy calls, even after a lost response.
pub(crate) async fn recover_confirmed(
    state: &AppState,
    record: &ComputeControlRecord,
) -> Result<ComputeControlRecord> {
    let observed_stop = record.action == ComputeControlAction::Stop && record.stage == "stopping";
    let observed_restart =
        record.action == ComputeControlAction::Restart && record.stage == "starting";
    if observed_restart {
        let context = record.execution_context().map_err(anyhow::Error::msg)?;
        let receipt = record
            .lease
            .as_ref()
            .context("Startup recovery lease is missing")?;
        ensure!(
            state
                .runtime()
                .validate_app_operation_receipt(&context, receipt)
                .await?,
            "Startup recovery lease is not confirmed"
        );
    }
    if observed_stop {
        let context = record.execution_context().map_err(anyhow::Error::msg)?;
        let receipt = record
            .lease
            .as_ref()
            .ok_or_else(|| anyhow!("Stop recovery lease is missing"))?;
        ensure!(
            state
                .runtime()
                .validate_app_operation_receipt(&context, receipt)
                .await?,
            "Stop recovery lease is not confirmed"
        );
        let absence = serde_json::from_value::<ComputeAbsenceCheckpoint>(record.checkpoint.clone())
            .ok()
            .filter(|proof| proof.compute_absent && proof.context == context);
        let orphan =
            serde_json::from_value::<BuilderOrphanStopTarget>(record.checkpoint.clone()).ok();
        let confirmed = if let Some(orphan) = orphan {
            orphan.validate().map_err(anyhow::Error::msg)?;
            ensure!(
                record.scope == UserAppOperationScope::Dev && orphan.context == context,
                "Orphan stop recovery context differs"
            );
            state
                .runtime()
                .confirm_builder_orphan_stopped(&orphan)
                .await?
        } else if absence.is_some() {
            match record.scope {
                UserAppOperationScope::Dev => state
                    .runtime()
                    .capture_builder_control(&context)
                    .await?
                    .workload
                    .is_none(),
                UserAppOperationScope::Prod => state.runtime().app_compute_absent(&context).await?,
                UserAppOperationScope::Application => false,
            }
        } else if record.scope == UserAppOperationScope::Dev {
            let target: BuilderControlTarget = serde_json::from_value(record.checkpoint.clone())?;
            ensure!(
                target.context == context,
                "Builder stop recovery context differs"
            );
            state
                .runtime()
                .reconcile_builder_compute_stop(&target)
                .await?
        } else {
            let target: UserAppMutationTarget = serde_json::from_value(record.checkpoint.clone())?;
            ensure!(target.context == context, "Stop recovery context differs");
            state.runtime().reconcile_app_compute_stop(&target).await?
        };
        ensure!(
            confirmed,
            "Original stop has no confirmed atomic runtime receipt; ownership retained"
        );
    }
    if observed_restart && record.scope == UserAppOperationScope::Dev {
        let target: BuilderControlTarget = serde_json::from_value(record.checkpoint.clone())?;
        ensure!(
            target.context == record.execution_context().map_err(anyhow::Error::msg)?,
            "Builder start recovery context differs"
        );
        let info = state
            .runtime()
            .reconcile_builder_compute_start(&target)
            .await?
            .ok_or_else(|| {
                anyhow!("Original builder startup receipt or readiness is not confirmed")
            })?;
        confirm_compute_builder_ready(state, record, info).await?;
        state
            .runtime()
            .reconcile_builder_compute_start(&target)
            .await?
            .context("Builder startup identity or readiness changed during recovery")?;
        // Runtime observation must not refresh a cache before the terminal CAS.
    }
    if record.action == ComputeControlAction::Restart && record.scope == UserAppOperationScope::Prod
    {
        let context = record.execution_context().map_err(anyhow::Error::msg)?;
        let expected: UserAppMutationTarget = serde_json::from_value(record.checkpoint.clone())?;
        ensure!(
            expected.context == context,
            "Compute recovery context differs"
        );
        if observed_restart {
            ensure!(
                state
                    .runtime()
                    .reconcile_app_compute_start(&expected)
                    .await?,
                "Original start receipt or current generation readiness is not confirmed"
            );
        }
        let before = super::app_adoption::capture_bound_app_target(state, &context).await?;
        ensure!(
            before.resource.uid == expected.resource.uid,
            "Production identity changed during recovery"
        );
        let status = state
            .runtime()
            .get_deployment_status(&record.app_id)
            .await?
            .ok_or_else(|| anyhow!("Production compute is absent during recovery"))?;
        ensure!(
            status.ready_replicas > 0,
            "Production compute is still not ready"
        );
        let after = super::app_adoption::capture_bound_app_target(state, &context).await?;
        ensure!(
            after.resource == before.resource,
            "Production changed while reading readiness"
        );
        if observed_restart {
            ensure!(
                state
                    .runtime()
                    .reconcile_app_compute_start(&expected)
                    .await?,
                "Original start receipt changed during readiness observation"
            );
        }
    }
    let completed = if observed_stop {
        state
            .userapp_store
            .finalize_observed_compute_stop(record)
            .await?
    } else if observed_restart {
        state
            .userapp_store
            .finalize_observed_compute_restart(record)
            .await?
    } else {
        state
            .userapp_store
            .finalize_confirmed_compute(record)
            .await?
    };
    // A release failure leaves terminal + receipt for the ordinary cleanup scan.
    // It must not turn the confirmed runtime outcome back into uncertainty.
    if let Some(receipt) = &completed.lease {
        let context = completed.execution_context().map_err(anyhow::Error::msg)?;
        match state
            .runtime()
            .release_app_operation_receipt(&context, receipt)
            .await
        {
            Ok(()) => {
                let identity = ComputeExecutorIdentity {
                    app_id: completed.app_id.clone(),
                    lifecycle_id: completed.lifecycle_id.clone(),
                    scope: completed.scope,
                    operation_id: completed.operation_id.clone(),
                    generation: completed.generation,
                    executor_id: context.executor_id,
                };
                if let Err(error) = state
                    .userapp_store
                    .forget_compute_lease(&identity, receipt)
                    .await
                {
                    tracing::warn!(%error, operation_id = %completed.operation_id, "Terminal compute receipt awaits cleanup");
                }
            }
            Err(error) => {
                tracing::warn!(%error, operation_id = %completed.operation_id, "Terminal compute lease awaits cleanup")
            }
        }
    }
    Ok(completed)
}

async fn progress(
    state: &AppState,
    record: &mut ComputeControlRecord,
    identity: &ComputeExecutorIdentity,
    stage: ComputeControlStage,
    checkpoint: serde_json::Value,
) -> Result<()> {
    *record = state
        .userapp_store
        .advance_compute_control(&ComputeControlProgress {
            identity: identity.clone(),
            expected_revision: record.revision,
            state: if stage == ComputeControlStage::Completed {
                ComputeControlState::Succeeded
            } else {
                ComputeControlState::Running
            },
            stage,
            checkpoint,
            error_code: None,
            error_message: None,
        })
        .await?;
    Ok(())
}

pub(crate) async fn execute_pending(state: &AppState, pending: ComputeControlRecord) -> Result<()> {
    let identity = ComputeExecutorIdentity {
        app_id: pending.app_id.clone(),
        lifecycle_id: pending.lifecycle_id.clone(),
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        scope: pending.scope,
        executor_id: uuid::Uuid::new_v4().to_string(),
    };
    let mut record = match state
        .userapp_store
        .claim_compute_control(&identity, pending.revision)
        .await
    {
        Ok(record) => record,
        Err(UserAppStoreError::VersionConflict) => return Ok(()), // another replica won
        Err(error) => return Err(error.into()),
    };
    let mut stage = ComputeControlStage::DrainingPrevious;
    let mut settled = true;
    let result = std::panic::AssertUnwindSafe(execute_claimed(
        state,
        &mut record,
        &identity,
        &mut stage,
        &mut settled,
    ))
    .catch_unwind()
    .await
    .unwrap_or_else(|_| {
        Err(anyhow!(
            "Compute execution panicked; inspect its captured runtime stage"
        ))
    });
    if let Err(error) = result {
        // A superseding Stop owns the head now; this executor cannot rewrite it.
        if state
            .userapp_store
            .check_compute_executor(&identity)
            .await
            .is_ok()
        {
            let failure = ComputeControlProgress {
                identity: identity.clone(),
                expected_revision: record.revision,
                state: ComputeControlState::RecoveryRequired,
                stage,
                checkpoint: record.checkpoint.clone(),
                error_code: Some(ERR_BACKEND_ERROR.into()),
                error_message: Some(format!("{error:#}")),
            };
            state
                .userapp_store
                .advance_compute_control(&failure)
                .await
                .context("Persist compute execution recovery state")?;
        }
        if settled
            && let Some(current) = state
                .userapp_store
                .get_compute_control(&identity.app_id, &identity.operation_id)
                .await?
            && current.state == ComputeControlState::Superseded
        {
            let evidence = if current.stage == "draining_previous" {
                ComputeControlDrainEvidence::NoMutationSubmitted
            } else {
                ComputeControlDrainEvidence::MutationCompleted {
                    checkpoint: current.checkpoint.clone(),
                }
            };
            let drained = state
                .userapp_store
                .acknowledge_compute_drain(&ComputeControlDrainAcknowledgement {
                    identity: identity.clone(),
                    expected_revision: current.revision,
                    evidence,
                })
                .await?;
            if let Some(ref receipt) = drained.lease {
                state
                    .runtime()
                    .release_app_operation_receipt(
                        &drained.execution_context().map_err(anyhow::Error::msg)?,
                        receipt,
                    )
                    .await?;
                state
                    .userapp_store
                    .forget_compute_lease(&identity, receipt)
                    .await?;
            }
        }
        return Err(error);
    }
    Ok(())
}

async fn execute_claimed(
    state: &AppState,
    record: &mut ComputeControlRecord,
    identity: &ComputeExecutorIdentity,
    stage: &mut ComputeControlStage,
    settled: &mut bool,
) -> Result<()> {
    // Pending business operations were atomically cancelled by claim. Running
    // writers must finish their actual calls and leave a terminal receipt.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        state.userapp_store.check_compute_executor(identity).await?;
        let mut drained = true;
        for id in &record.interrupted_operations {
            if let Some(mut old) = state
                .userapp_store
                .get_operation(&record.app_id, id)
                .await?
            {
                if !old.state.is_terminal()
                    && userapp_operation_has_drain_evidence(&old)
                    && !old.kind.ends_lifecycle()
                {
                    match state
                        .userapp_store
                        .finalize_compute_interrupted_operation(identity, &old)
                        .await
                    {
                        Ok(finalized) => old = finalized,
                        Err(UserAppStoreError::VersionConflict) => {
                            drained = false;
                            continue;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                // Do not depend on another scanner slot to release terminal
                // leases: all recovery slots may themselves be draining controls.
                if old.state.is_terminal()
                    && let Some(binding) = state
                        .userapp_store
                        .get_operation_lease(&record.app_id, id)
                        .await?
                {
                    state
                        .runtime()
                        .release_app_operation_receipt(&binding.context, &binding.receipt)
                        .await?;
                    state.userapp_store.forget_operation_lease(&binding).await?;
                }
                drained &= old.state.is_terminal()
                    && state
                        .userapp_store
                        .get_operation_lease(&record.app_id, id)
                        .await?
                        .is_none();
            } else if let Some(mut old) = state
                .userapp_store
                .get_compute_control(&record.app_id, id)
                .await?
            {
                if old.state == ComputeControlState::Superseded
                    && old.action == ComputeControlAction::Restart
                    && (matches!(old.stage.as_str(), "stopped" | "verifying")
                        || (matches!(old.stage.as_str(), "starting" | "stopping")
                            && (old.has_conditional_compute_write()
                                || old.has_docker_compute_target())))
                    && old.lease.is_some()
                {
                    match reconcile_superseded(state, &old).await {
                        Ok(closed) => old = closed,
                        Err(error)
                            if matches!(
                                error.downcast_ref::<UserAppStoreError>(),
                                Some(UserAppStoreError::VersionConflict)
                            ) =>
                        {
                            drained = false;
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
                if matches!(
                    old.state,
                    ComputeControlState::Succeeded | ComputeControlState::Failed
                ) && let Some(receipt) = old.lease.as_ref()
                {
                    let context = old.execution_context().map_err(anyhow::Error::msg)?;
                    state
                        .runtime()
                        .release_app_operation_receipt(&context, receipt)
                        .await?;
                    state
                        .userapp_store
                        .forget_compute_lease(
                            &ComputeExecutorIdentity {
                                app_id: old.app_id.clone(),
                                lifecycle_id: old.lifecycle_id.clone(),
                                scope: old.scope,
                                operation_id: old.operation_id.clone(),
                                generation: old.generation,
                                executor_id: context.executor_id,
                            },
                            receipt,
                        )
                        .await?;
                    old.lease = None;
                }
                drained &= (matches!(
                    old.state,
                    ComputeControlState::Succeeded | ComputeControlState::Failed
                ) || (old.state == ComputeControlState::Superseded
                    && old.executor_id.is_none()))
                    && old.lease.is_none();
            } else {
                return Err(anyhow!("Interrupted operation evidence is missing: {id}"));
            }
        }
        if drained {
            break;
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "Previous execution is not yet reconciled; retain the original operation and physical receipt"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let lease = match identity.scope {
        UserAppOperationScope::Dev => {
            state
                .runtime()
                .acquire_builder_operation(&record.app_id)
                .await?
        }
        UserAppOperationScope::Prod => state
            .runtime()
            .acquire_app_operation(&record.app_id)
            .await?
            .ok_or_else(|| anyhow!("Runtime did not return a physical operation lease"))?,
        UserAppOperationScope::Application => {
            return Err(anyhow!("Compute control requires dev or prod scope"));
        }
    };
    let mut lease = BuilderOperation::new(lease);
    // Validate after acquiring the physical lease: waiting for another writer
    // must not turn an earlier storage observation into permission to restart.
    // Stop deliberately has no storage prerequisite.
    if record.action == ComputeControlAction::Restart {
        state
            .app_service
            .verify_recovered_storage(&record.app_id, identity.scope)
            .await?;
        state.userapp_store.check_compute_executor(identity).await?;
    }
    let receipt = lease.receipt().map_err(anyhow::Error::msg)?;
    *record = state
        .userapp_store
        .bind_compute_lease(identity, &receipt)
        .await?;
    lease
        .begin_external_mutation()
        .map_err(anyhow::Error::msg)?;
    let context = record.execution_context().map_err(anyhow::Error::msg)?;
    if record.action == ComputeControlAction::Stop
        && identity.scope == UserAppOperationScope::Dev
        && let Some(orphan) = super::adoption::capture_bound_orphan_stop(state, &context).await?
    {
        let checkpoint = serde_json::to_value(&orphan)?;
        progress(
            state,
            record,
            identity,
            ComputeControlStage::Stopping,
            checkpoint.clone(),
        )
        .await?;
        *stage = ComputeControlStage::Stopping;
        state.userapp_store.check_compute_executor(identity).await?;
        *settled = false;
        state.runtime().stop_builder_orphan(&orphan).await?;
        *settled = true;
        progress(
            state,
            record,
            identity,
            ComputeControlStage::Stopped,
            checkpoint.clone(),
        )
        .await?;
        *stage = ComputeControlStage::Stopped;
        crate::userapp_forward::invalidate_probe_cache(&record.app_id);
        progress(
            state,
            record,
            identity,
            ComputeControlStage::Completed,
            checkpoint,
        )
        .await?;
        *stage = ComputeControlStage::Completed;
        lease
            .finish_external_mutation()
            .await
            .map_err(anyhow::Error::msg)?;
        state
            .userapp_store
            .forget_compute_lease(identity, &receipt)
            .await?;
        return Ok(());
    }
    let absent = if record.action == ComputeControlAction::Stop {
        match identity.scope {
            UserAppOperationScope::Dev => super::adoption::capture_bound_target(state, &context)
                .await?
                .workload
                .is_none(),
            UserAppOperationScope::Prod => state.runtime().app_compute_absent(&context).await?,
            UserAppOperationScope::Application => false,
        }
    } else {
        false
    };
    if absent {
        state.userapp_store.check_compute_executor(identity).await?;
        let evidence = serde_json::to_value(ComputeAbsenceCheckpoint {
            context,
            compute_absent: true,
        })?;
        // Follow the same durable stage sequence; this branch issues no writes
        // to compute, never creates a missing container, and leaves volumes alone.
        for next in [
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Completed,
        ] {
            progress(state, record, identity, next, evidence.clone()).await?;
            *stage = next;
        }
        lease
            .finish_external_mutation()
            .await
            .map_err(anyhow::Error::msg)?;
        state
            .userapp_store
            .forget_compute_lease(identity, &receipt)
            .await?;
        return Ok(());
    }
    let mut target = if identity.scope == UserAppOperationScope::Dev {
        let captured = super::adoption::capture_bound_target(state, &context).await?;
        builder_compute_checkpoint(
            state,
            &captured,
            record.action == ComputeControlAction::Restart,
        )
        .await?
    } else {
        let target = super::app_adoption::capture_bound_app_target(state, &context).await?;
        if record.action == ComputeControlAction::Restart {
            let mut prepared_target = state.runtime().prepare_app_compute_start(&target).await?;
            // Freeze the platform-default image into the very first checkpoint:
            // every later stage (including crash resume) replays this frozen
            // value instead of re-reading the env.
            prepared_target.restart_image = prod_restart_image();
            let mut prepared = serde_json::to_value(prepared_target)?;
            if let Some(template) = state.runtime().archive_app_restart(&target).await? {
                prepared["app_restart_template"] = serde_json::to_value(template)?;
            }
            prepared
        } else {
            serde_json::to_value(target)?
        }
    };
    if identity.scope == UserAppOperationScope::Dev
        && record.action == ComputeControlAction::Restart
    {
        let captured: BuilderControlTarget = serde_json::from_value(target.clone())?;
        state.userapp_store.check_compute_executor(identity).await?;
        if let Some(template) = state.runtime().archive_builder_restart(&captured).await? {
            target["builder_restart_template"] = serde_json::to_value(template)?;
        }
    }
    progress(
        state,
        record,
        identity,
        ComputeControlStage::Stopping,
        target.clone(),
    )
    .await?;
    *stage = ComputeControlStage::Stopping;
    state.userapp_store.check_compute_executor(identity).await?;
    *settled = false;
    if identity.scope == UserAppOperationScope::Dev {
        let target: BuilderControlTarget = serde_json::from_value(target.clone())?;
        state
            .runtime()
            .apply_builder_control(&target, false)
            .await?;
    } else {
        let target: UserAppMutationTarget = serde_json::from_value(target.clone())?;
        state.runtime().stop_app_target(&target, false).await?;
        state.runtime().confirm_app_compute_stopped(&target).await?;
    }
    *settled = true;
    progress(
        state,
        record,
        identity,
        ComputeControlStage::Stopped,
        target.clone(),
    )
    .await?;
    *stage = ComputeControlStage::Stopped;
    if identity.scope == UserAppOperationScope::Dev {
        crate::userapp_forward::invalidate_probe_cache(&record.app_id);
    }
    if record.action == ComputeControlAction::Restart {
        // Scale-down changed resourceVersion. Refresh only the SAME physical UID.
        let fresh = if identity.scope == UserAppOperationScope::Dev {
            let old: BuilderControlTarget = serde_json::from_value(target.clone())?;
            let mut fresh = super::adoption::capture_bound_target(state, &context).await?;
            if old.workload.as_ref().map(|r| &r.uid) != fresh.workload.as_ref().map(|r| &r.uid) {
                let template: BuilderRestartTemplate = serde_json::from_value(
                    target
                        .get("builder_restart_template")
                        .context("Original restart template is unavailable")?
                        .clone(),
                )?;
                ensure!(template.source == old, "Restart archive source differs");
                state.userapp_store.check_compute_executor(identity).await?;
                *settled = false;
                fresh = state.runtime().restore_builder_restart(&template).await?;
                *settled = true;
            }
            ensure!(
                fresh.workload.is_some(),
                "Restart cannot create an absent builder"
            );
            let prepared = builder_compute_checkpoint(state, &fresh, true).await?;
            verify_builder_volume_witness(&target, &prepared)?;
            prepared
        } else {
            let old: UserAppMutationTarget = serde_json::from_value(target.clone())?;
            let mut fresh = super::app_adoption::capture_bound_app_target(state, &context).await?;
            if old.resource.uid != fresh.resource.uid {
                // Scale-down (or external deletion) replaced the controller.
                // Restore a zero-replica replacement from the operation-bound
                // archive; the Starting CAS below must win before it starts.
                // Docker prod archives nothing (inline env secrets must never
                // reach disk); its recovery path is explicit redeployment or
                // the adoption endpoint, not a template replay.
                let template: AppRestartTemplate = serde_json::from_value(
                    target
                        .get("app_restart_template")
                        .context(
                            "Original restart template is unavailable; this backend does not \
                             archive application restarts — redeploy explicitly or adopt the \
                             replacement controller",
                        )?
                        .clone(),
                )?;
                ensure!(template.source == old, "Restart archive source differs");
                state.userapp_store.check_compute_executor(identity).await?;
                *settled = false;
                fresh = state.runtime().restore_app_restart(&template).await?;
                *settled = true;
            }
            let mut prepared = state.runtime().prepare_app_compute_start(&fresh).await?;
            let prior: UserAppComputeStartTarget = serde_json::from_value(target.clone())?;
            prior
                .verify_same_volumes(&prepared)
                .map_err(anyhow::Error::msg)?;
            prepared.restart_image = prior.restart_image.clone();
            let mut fresh = serde_json::to_value(prepared)?;
            // Keep the restart archive reachable from every later stage: an
            // interrupted Starting resume still needs its recovery source.
            if let Some(template) = target.get("app_restart_template") {
                fresh["app_restart_template"] = template.clone();
            }
            fresh
        };
        progress(
            state,
            record,
            identity,
            ComputeControlStage::Starting,
            fresh.clone(),
        )
        .await?;
        *stage = ComputeControlStage::Starting;
        state
            .app_service
            .verify_recovered_storage(&record.app_id, identity.scope)
            .await?;
        state.userapp_store.check_compute_executor(identity).await?;
        *settled = false;
        if identity.scope == UserAppOperationScope::Dev {
            let target: BuilderControlTarget = serde_json::from_value(fresh.clone())?;
            let info = state
                .runtime()
                .start_builder_control(&target)
                .await?
                .ok_or_else(|| anyhow!("Restart did not return a builder"))?;
            *settled = true;
            let info = confirm_compute_builder_ready(state, record, info).await?;
            state.userapp_store.check_compute_executor(identity).await?;
            super::register_builder(state, &record.app_id, &info)?;
        } else {
            state
                .runtime()
                .start_app_compute(&serde_json::from_value::<UserAppComputeStartTarget>(
                    fresh.clone(),
                )?)
                .await?;
        }
        *settled = true;
        progress(
            state,
            record,
            identity,
            ComputeControlStage::Verifying,
            fresh,
        )
        .await?;
        *stage = ComputeControlStage::Verifying;
        if identity.scope == UserAppOperationScope::Prod {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
            loop {
                state.userapp_store.check_compute_executor(identity).await?;
                let expected: UserAppMutationTarget =
                    serde_json::from_value(record.checkpoint.clone())?;
                let actual = super::app_adoption::capture_bound_app_target(state, &context).await?;
                ensure!(
                    actual.resource.uid == expected.resource.uid,
                    "Production identity changed during verification"
                );
                let status = state
                    .runtime()
                    .get_deployment_status(&record.app_id)
                    .await?
                    .ok_or_else(|| anyhow!("Restarted production compute disappeared"))?;
                if status.ready_replicas > 0 {
                    // The status endpoint is keyed by application, not workload
                    // UID. Fence its observation with another live identity read.
                    let after =
                        super::app_adoption::capture_bound_app_target(state, &context).await?;
                    ensure!(
                        after.resource == actual.resource,
                        "Production changed while reading restart readiness"
                    );
                    state.userapp_store.check_compute_executor(identity).await?;
                    break;
                }
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "Production compute did not become ready; management and application startup require inspection"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    progress(
        state,
        record,
        identity,
        ComputeControlStage::Completed,
        record.checkpoint.clone(),
    )
    .await?;
    lease
        .finish_external_mutation()
        .await
        .map_err(anyhow::Error::msg)?;
    state
        .userapp_store
        .forget_compute_lease(identity, &receipt)
        .await?;
    Ok(())
}

/// Restart-path platform image resolution: a missing or blank env degrades to
/// `None` (plain restart) with a warning — restart availability must not brick
/// on deployment config gaps (create/update keep their fail-fast contract).
fn prod_restart_image() -> Option<String> {
    let resolved = std::env::var("RCODER_RUNTIME_IMAGE_DIGEST")
        .ok()
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if resolved.is_none() {
        tracing::warn!(
            "RCODER_RUNTIME_IMAGE_DIGEST missing/blank: compute restart keeps the current image"
        );
    }
    resolved
}

/// Continue a proven stopped restart under the same durable lease. A CAS
/// selects one starter; losing observers never submit the runtime request.
pub(crate) async fn resume_restart_start(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<()> {
    if snapshot.scope == UserAppOperationScope::Dev {
        return resume_builder_start(state, snapshot).await;
    }
    let context = snapshot.execution_context().map_err(anyhow::Error::msg)?;
    let receipt = snapshot
        .lease
        .as_ref()
        .context("Restart lease is missing")?;
    ensure!(
        state
            .runtime()
            .validate_app_operation_receipt(&context, receipt)
            .await?,
        "Original restart lease is not confirmed"
    );
    let prior: UserAppComputeStartTarget = serde_json::from_value(snapshot.checkpoint.clone())?;
    ensure!(
        prior.target.context == context,
        "Restart context differs from original capture"
    );
    let prepared = if snapshot.stage == "starting" {
        if state
            .runtime()
            .reconcile_app_compute_start(&prior.target)
            .await?
        {
            recover_confirmed(state, snapshot).await?;
            return Ok(());
        }
        state.runtime().prepare_app_compute_start_retry(&prior).await?
            .context("Original startup is committed but not ready; continue observing the same operation")?
    } else {
        ensure!(
            state
                .runtime()
                .reconcile_app_compute_stop(&prior.target)
                .await?,
            "Original stopped boundary is not confirmed"
        );
        let mut target = super::app_adoption::capture_bound_app_target(state, &context).await?;
        if target.resource.uid != prior.target.resource.uid {
            let template: AppRestartTemplate = serde_json::from_value(
                snapshot
                    .checkpoint
                    .get("app_restart_template")
                    .context(
                        "Original restart template is unavailable; this backend does not \
                         archive application restarts — redeploy explicitly or adopt the \
                         replacement controller",
                    )?
                    .clone(),
            )?;
            ensure!(
                template.source == prior.target,
                "Restart archive source differs"
            );
            // The durable stopped boundary already confirmed old compute exited.
            // Restore only a zero-replica controller; the CAS below must
            // succeed before any new business instance can start.
            target = state.runtime().restore_app_restart(&template).await?;
        }
        let mut prepared = state.runtime().prepare_app_compute_start(&target).await?;
        prior
            .verify_same_volumes(&prepared)
            .map_err(anyhow::Error::msg)?;
        prepared.restart_image = prior.restart_image.clone();
        prepared
    };
    ensure!(
        state
            .runtime()
            .validate_app_operation_receipt(&context, receipt)
            .await?,
        "Restart lease changed before continuation claim"
    );
    let record = state
        .userapp_store
        .resume_compute_restart_start(snapshot, &prepared.target)
        .await?;
    run_restart_continuation(state, record, RestartContinuation::Prod(prepared)).await
}

enum RestartContinuation {
    Prod(UserAppComputeStartTarget),
    Dev(BuilderControlTarget),
}

async fn run_restart_continuation(
    state: &AppState,
    mut record: ComputeControlRecord,
    prepared: RestartContinuation,
) -> Result<()> {
    let context = record.execution_context().map_err(anyhow::Error::msg)?;
    let checkpoint = match &prepared {
        RestartContinuation::Prod(target) => {
            let mut value = serde_json::to_value(target)?;
            // Preserve the restart archive reference through later stages.
            if let Some(template) = record.checkpoint.get("app_restart_template") {
                value["app_restart_template"] = template.clone();
            }
            value
        }
        RestartContinuation::Dev(_) => record.checkpoint.clone(),
    };
    let identity = ComputeExecutorIdentity {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        operation_id: record.operation_id.clone(),
        scope: record.scope,
        generation: record.generation,
        executor_id: context.executor_id.clone(),
    };
    let mut unknown_write = false;
    let result: Result<()> = async {
        state
            .app_service
            .verify_recovered_storage(&record.app_id, identity.scope)
            .await?;
        state
            .userapp_store
            .check_compute_executor(&identity)
            .await?;
        unknown_write = true;
        match &prepared {
            RestartContinuation::Prod(target) => state.runtime().start_app_compute(target).await?,
            RestartContinuation::Dev(target) => {
                let info = state
                    .runtime()
                    .start_builder_control(target)
                    .await?
                    .ok_or_else(|| {
                        anyhow!("Builder continuation did not return a ready instance")
                    })?;
                unknown_write = false;
                let info = confirm_compute_builder_ready(state, &record, info).await?;
                state
                    .userapp_store
                    .check_compute_executor(&identity)
                    .await?;
                super::register_builder(state, &record.app_id, &info)?;
            }
        }
        unknown_write = false;
        progress(
            state,
            &mut record,
            &identity,
            ComputeControlStage::Verifying,
            checkpoint,
        )
        .await?;
        // Readiness may still be pending. Persisted Verifying lets the existing
        // scanner finish later without launching a second startup.
        recover_confirmed(state, &record).await?;
        // The restart reached its terminal boundary; the private archive has
        // no remaining recovery value. Records that crash before this point
        // stay covered by the scan-based GC instead.
        if record.action == ComputeControlAction::Restart {
            let cleanup = match record.scope {
                UserAppOperationScope::Dev => {
                    match record.checkpoint.get("builder_restart_template") {
                        Some(value) => {
                            let template: BuilderRestartTemplate =
                                serde_json::from_value(value.clone()).map_err(|error| {
                                    anyhow::anyhow!("Decode builder restart archive: {error}")
                                })?;
                            Some(
                                state
                                    .runtime()
                                    .cleanup_builder_restart_archive(&template)
                                    .await,
                            )
                        }
                        None => None,
                    }
                }
                UserAppOperationScope::Prod => match record.checkpoint.get("app_restart_template")
                {
                    Some(value) => {
                        let template: AppRestartTemplate =
                            serde_json::from_value(value.clone()).map_err(|error| {
                                anyhow::anyhow!("Decode application restart archive: {error}")
                            })?;
                        Some(state.runtime().cleanup_app_restart_archive(&template).await)
                    }
                    None => None,
                },
                UserAppOperationScope::Application => None,
            };
            if let Some(result) = cleanup
                && let Err(error) = result
            {
                tracing::warn!(%error, operation_id = %record.operation_id, "Restart archive awaits scan cleanup");
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = &result {
        if state
            .userapp_store
            .check_compute_executor(&identity)
            .await
            .is_ok()
        {
            state
                .userapp_store
                .advance_compute_control(&ComputeControlProgress {
                    identity: identity.clone(),
                    expected_revision: record.revision,
                    state: ComputeControlState::RecoveryRequired,
                    stage: if record.stage == "verifying" {
                        ComputeControlStage::Verifying
                    } else {
                        ComputeControlStage::Starting
                    },
                    checkpoint: record.checkpoint.clone(),
                    error_code: Some(ERR_BACKEND_ERROR.into()),
                    error_message: Some(format!("{error:#}")),
                })
                .await?;
        } else if !unknown_write
            && let Some(current) = state
                .userapp_store
                .get_compute_control(&identity.app_id, &identity.operation_id)
                .await?
            && current.state == ComputeControlState::Superseded
        {
            state
                .userapp_store
                .acknowledge_compute_drain(&ComputeControlDrainAcknowledgement {
                    identity,
                    expected_revision: current.revision,
                    evidence: ComputeControlDrainEvidence::MutationCompleted {
                        checkpoint: current.checkpoint,
                    },
                })
                .await?;
        }
    }
    result
}

/// Close only a superseded execution with a durable boundary or a verified
/// remote conditional-write fence. The store CAS is shared by scanning and Stop.
pub(crate) async fn reconcile_superseded(
    state: &AppState,
    old: &ComputeControlRecord,
) -> Result<ComputeControlRecord> {
    ensure!(
        old.state == ComputeControlState::Superseded && old.action == ComputeControlAction::Restart,
        "Compute operation is not a superseded restart"
    );
    let context = old.execution_context().map_err(anyhow::Error::msg)?;
    let evidence = if matches!(old.stage.as_str(), "starting" | "stopping")
        && old.has_docker_compute_target()
    {
        let receipt = old
            .lease
            .as_ref()
            .context("Original Docker compute lease missing")?;
        ensure!(
            state
                .runtime()
                .validate_app_operation_receipt(&context, receipt)
                .await?,
            "Original Docker compute lease cannot be verified"
        );
        let acknowledged = match old.scope {
            UserAppOperationScope::Dev => {
                state
                    .runtime()
                    .builder_compute_write_acknowledged(
                        &serde_json::from_value::<BuilderControlTarget>(old.checkpoint.clone())?,
                        old.stage == "starting",
                    )
                    .await?
            }
            UserAppOperationScope::Prod => {
                state
                    .runtime()
                    .app_compute_write_acknowledged(
                        &serde_json::from_value::<UserAppMutationTarget>(old.checkpoint.clone())?,
                        old.stage == "starting",
                    )
                    .await?
            }
            UserAppOperationScope::Application => false,
        };
        ensure!(
            acknowledged,
            "Superseded Docker write has no durable acknowledgement"
        );
        ComputeControlDrainEvidence::PersistedRuntimeAcknowledgement {
            checkpoint: old.checkpoint.clone(),
        }
    } else if matches!(old.stage.as_str(), "starting" | "stopping") {
        ensure!(
            old.has_conditional_compute_write(),
            "Original conditional-write capability missing"
        );
        let receipt = old
            .lease
            .as_ref()
            .ok_or_else(|| anyhow!("Original compute lease missing"))?;
        ensure!(
            state
                .runtime()
                .validate_app_operation_receipt(&context, receipt)
                .await?,
            "Original compute lease cannot be verified"
        );
        let fenced = if old.scope == UserAppOperationScope::Dev {
            let target: BuilderControlTarget = serde_json::from_value(old.checkpoint.clone())?;
            ensure!(target.context == context, "Builder fence context differs");
            state.runtime().fence_builder_compute_write(&target).await?
        } else {
            let captured: UserAppComputeStartTarget =
                serde_json::from_value(old.checkpoint.clone())?;
            ensure!(
                captured.target.context == context,
                "Startup fence context differs"
            );
            if old.stage == "stopping" {
                state.runtime().fence_app_compute_stop(&captured).await?
            } else {
                state.runtime().fence_app_compute_start(&captured).await?
            }
        };
        ensure!(
            fenced,
            "Superseded conditional compute write is not yet fenced"
        );
        if old.stage == "stopping" {
            ComputeControlDrainEvidence::ConditionalStopFenced {
                checkpoint: old.checkpoint.clone(),
            }
        } else {
            ComputeControlDrainEvidence::ConditionalStartupFenced {
                checkpoint: old.checkpoint.clone(),
            }
        }
    } else {
        ComputeControlDrainEvidence::PersistedMutationBoundary {
            checkpoint: old.checkpoint.clone(),
        }
    };
    // Completed boundaries need no new runtime write. Starting
    // instead requires the exact remote precondition fence above;
    // supersession alone cannot drain an in-flight request.
    state
        .userapp_store
        .acknowledge_compute_drain(&ComputeControlDrainAcknowledgement {
            identity: ComputeExecutorIdentity {
                app_id: old.app_id.clone(),
                lifecycle_id: old.lifecycle_id.clone(),
                scope: old.scope,
                operation_id: old.operation_id.clone(),
                generation: old.generation,
                executor_id: context.executor_id,
            },
            expected_revision: old.revision,
            evidence,
        })
        .await
        .map_err(Into::into)
}

async fn resume_builder_start(state: &AppState, snapshot: &ComputeControlRecord) -> Result<()> {
    ensure!(
        snapshot.stage == "stopped"
            || (snapshot.stage == "starting" && snapshot.has_conditional_compute_write()),
        "Builder continuation requires a stopped boundary or conditional-write witness"
    );
    let context = snapshot.execution_context().map_err(anyhow::Error::msg)?;
    let receipt = snapshot
        .lease
        .as_ref()
        .context("Builder continuation lease missing")?;
    let old: BuilderControlTarget = serde_json::from_value(snapshot.checkpoint.clone())?;
    ensure!(
        old.context == context,
        "Builder continuation context differs"
    );
    ensure!(
        state
            .runtime()
            .validate_app_operation_receipt(&context, receipt)
            .await?,
        "Original builder lease is not confirmed"
    );
    let target = if snapshot.stage == "starting" {
        if state
            .runtime()
            .reconcile_builder_compute_start(&old)
            .await?
            .is_some()
        {
            recover_confirmed(state, snapshot).await?;
            return Ok(());
        }
        state
            .runtime()
            .prepare_builder_compute_retry(&old)
            .await?
            .context("Original builder startup is committed but not ready; keep observing")?
    } else {
        let fresh = super::adoption::capture_bound_target(state, &context).await?;
        if fresh.workload.as_ref().map(|r| &r.uid) != old.workload.as_ref().map(|r| &r.uid) {
            let template: BuilderRestartTemplate = serde_json::from_value(
                snapshot
                    .checkpoint
                    .get("builder_restart_template")
                    .context("Original restart template is unavailable")?
                    .clone(),
            )?;
            ensure!(template.source == old, "Restart archive source differs");
            // The durable stopped boundary already confirms old compute exited.
            // Restore only a zero-replica controller; the CAS below must succeed
            // before any new business instance can start.
            state.runtime().restore_builder_restart(&template).await?
        } else {
            ensure!(
                state.runtime().reconcile_builder_compute_stop(&old).await?,
                "Original builder stop receipt is not confirmed"
            );
            fresh
        }
    };
    state
        .app_service
        .verify_recovered_storage(&snapshot.app_id, UserAppOperationScope::Dev)
        .await?;
    ensure!(
        target.pod.is_none(),
        "Builder Pod appeared before continuation"
    );
    ensure!(
        state
            .runtime()
            .validate_app_operation_receipt(&context, receipt)
            .await?,
        "Builder lease changed before continuation claim"
    );
    let prepared = builder_compute_checkpoint(state, &target, true).await?;
    verify_builder_volume_witness(&snapshot.checkpoint, &prepared)?;
    let record = state
        .userapp_store
        .resume_builder_restart_start(snapshot, &target)
        .await?;
    run_restart_continuation(state, record, RestartContinuation::Dev(target)).await
}

/// Only the read-only readiness future is cancelled. Runtime writes have already
/// returned before callers enter here and retain their original receipt.
async fn confirm_compute_builder_ready(
    state: &AppState,
    record: &ComputeControlRecord,
    info: ContainerBasicInfo,
) -> Result<ContainerBasicInfo> {
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(state.config.userapp_storage.ensure_timeout_seconds);
    let observe_owner = async {
        loop {
            let current = state
                .userapp_store
                .get_compute_control(&record.app_id, &record.operation_id)
                .await?;
            if current.as_ref() != Some(record) {
                return Err::<(), anyhow::Error>(anyhow!(
                    "Compute operation changed while waiting for builder readiness"
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    tokio::time::timeout_at(deadline, async {
        tokio::select! {
            result = super::confirm_builder_ready(state, &record.app_id, &record.app_id, info, deadline) => result,
            result = observe_owner => {
                result?;
                Err(anyhow!("Compute readiness observation stopped"))
            }
        }
    }).await.context("Compute builder readiness deadline exceeded")?
}

async fn builder_compute_checkpoint(
    state: &AppState,
    target: &BuilderControlTarget,
    capture_volumes: bool,
) -> Result<serde_json::Value> {
    let mut checkpoint = serde_json::to_value(target)?;
    if state.runtime().supports_builder_compute_fencing() {
        checkpoint["builder_compute_single_write"] = serde_json::Value::Bool(true);
    }
    if capture_volumes {
        checkpoint["builder_volumes"] = serde_json::to_value(
            state
                .runtime()
                .capture_builder_compute_volumes(target)
                .await?,
        )?;
    }
    Ok(checkpoint)
}

fn verify_builder_volume_witness(
    original: &serde_json::Value,
    prepared: &serde_json::Value,
) -> Result<()> {
    let old = original
        .get("builder_volumes")
        .context("Original builder volume witness missing; explicit recovery required")?;
    let new = prepared
        .get("builder_volumes")
        .context("Prepared builder volume witness missing")?;
    let old: Vec<AppResourceIdentity> = serde_json::from_value(old.clone())?;
    let new: Vec<AppResourceIdentity> = serde_json::from_value(new.clone())?;
    ensure!(
        old == new,
        "Builder workspace volume identity changed during restart"
    );
    if prepared
        .get("builder_compute_single_write")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        ensure!(
            !old.is_empty(),
            "Conditional builder restart requires workspace volume evidence"
        );
    }
    Ok(())
}
