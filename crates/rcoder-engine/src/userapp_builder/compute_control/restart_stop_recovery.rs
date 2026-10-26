//! Recover the original Docker Restart Stop result under its actual retained FD.
//! No Stop, DELETE, replacement creation protocol or early Starting claim lives here.
use super::*;

pub(super) fn eligible(record: &ComputeControlRecord) -> bool {
    if record.scope != UserAppOperationScope::Dev
        || record.action != ComputeControlAction::Restart
        || !record.has_docker_compute_target()
    {
        return false;
    }
    match record.state {
        ComputeControlState::Running | ComputeControlState::RecoveryRequired => {}
        ComputeControlState::Pending
        | ComputeControlState::Succeeded
        | ComputeControlState::Failed
        | ComputeControlState::Superseded => return false,
    }
    match ComputeControlStage::from_storage_value(&record.stage) {
        Some(ComputeControlStage::Stopping) => true,
        Some(
            ComputeControlStage::DrainingPrevious
            | ComputeControlStage::Stopped
            | ComputeControlStage::Starting
            | ComputeControlStage::Verifying
            | ComputeControlStage::Completed,
        )
        | None => false,
    }
}

pub(super) async fn validate_guard(
    guard: &dyn AppOperationLease,
    record: &ComputeControlRecord,
) -> Result<()> {
    ensure!(
        guard.receipt().as_ref() == record.lease.as_ref() && record.lease.is_some(),
        "Retained compute guard differs from the original receipt"
    );
    ensure!(
        guard
            .validate_retained()
            .await
            .map_err(anyhow::Error::msg)?,
        "Original compute fd/path/token is no longer current"
    );
    Ok(())
}

/// The Kubernetes Restart stop boundary. The original Docker FD recovery above
/// owns Container targets exclusively, so a StatefulSet target stuck at
/// Stopping (executor died between the stage write and the Stopped write)
/// previously had no recovery branch at all and blocked its scope forever.
pub(super) fn kubernetes_eligible(record: &ComputeControlRecord) -> bool {
    if record.scope != UserAppOperationScope::Dev || record.action != ComputeControlAction::Restart
    {
        return false;
    }
    match record.state {
        ComputeControlState::Running | ComputeControlState::RecoveryRequired => {}
        ComputeControlState::Pending
        | ComputeControlState::Succeeded
        | ComputeControlState::Failed
        | ComputeControlState::Superseded => return false,
    }
    if record.stage != ComputeControlStage::Stopping.as_str() {
        return false;
    }
    let Ok(context) = record.execution_context() else {
        return false;
    };
    BuilderControlTarget::from_observation_checkpoint(&record.checkpoint)
        .ok()
        .is_some_and(|target| {
            target.context == context
                && target
                    .workload
                    .as_ref()
                    .is_some_and(|workload| workload.kind == AppResourceKind::StatefulSet)
        })
}

/// Confirm the original Kubernetes stop boundary through read-only runtime
/// evidence, then hand the durable stopped stage to the existing restart
/// continuation branch (resume_restart_start on the next scan). No mutation is
/// re-issued and no lease is retaken here; the exact record CAS in storage
/// protects against stale observers.
pub(super) async fn confirm_kubernetes_stop(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<()> {
    ensure!(
        kubernetes_eligible(snapshot),
        "Restart is not at the Kubernetes stop boundary"
    );
    let context = snapshot.execution_context().map_err(anyhow::Error::msg)?;
    snapshot
        .lease
        .as_ref()
        .context("Original Kubernetes compute lease missing")?;
    let target = BuilderControlTarget::from_observation_checkpoint(&snapshot.checkpoint)?;
    ensure!(
        target.context == context,
        "Restart stop target context differs"
    );
    // Read-only evidence, strongest first: the platform stop receipt for this
    // exact operation context, else an exact UID-matched externally stopped
    // controller. Neither authorizes any new physical mutation.
    let evidence = if state
        .runtime()
        .reconcile_builder_compute_stop(&target)
        .await?
    {
        KubernetesRestartStopEvidence::StopReceiptAcknowledged
    } else if state
        .runtime()
        .observe_builder_compute_stopped(&target)
        .await?
    {
        KubernetesRestartStopEvidence::ObservedStopped
    } else {
        tracing::debug!(
            operation_id = %snapshot.operation_id,
            app_id = %snapshot.app_id,
            "Kubernetes Restart stop side is not yet confirmable; retrying on the next scan"
        );
        return Ok(());
    };
    let record = state
        .userapp_store
        .confirm_kubernetes_restart_stopped(snapshot, evidence)
        .await?;
    tracing::info!(
        operation_id = %record.operation_id,
        app_id = %record.app_id,
        evidence = ?evidence,
        "Kubernetes Restart stop boundary confirmed; start continuation resumes on the next scan"
    );
    Ok(())
}

fn identity(record: &ComputeControlRecord) -> Result<ComputeExecutorIdentity> {
    let context = record.execution_context().map_err(anyhow::Error::msg)?;
    Ok(ComputeExecutorIdentity {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        scope: record.scope,
        operation_id: record.operation_id.clone(),
        generation: record.generation,
        executor_id: context.executor_id,
    })
}

/// Only the old Restart needs its old recipe. Once exact old CID absence is
/// confirmed, a missing/unreadable/positively unavailable recipe can close that
/// old operation as Failed, preserving its timeout and archive reference. A
/// normal complete current Source request then follows existing admission.
async fn settle_unrebuildable_absent_recipe(
    state: &AppState,
    current: &mut ComputeControlRecord,
    observation: &BuilderRestartStopObservation,
    guard: &dyn AppOperationLease,
) -> Result<bool> {
    if observation.kind != BuilderRestartStopKind::CapturedContainerAbsent {
        return Ok(false);
    }
    ensure!(
        current.stage == ComputeControlStage::Stopping.as_str(),
        "Recipe settlement cannot infer whether an old Stopped row submitted Create"
    );
    let reference = current.checkpoint.get("builder_restart_template");
    let unavailable = match reference
        .map(|value| serde_json::from_value::<BuilderRestartTemplate>(value.clone()))
    {
        None | Some(Err(_)) => None,
        Some(Ok(template)) => {
            let bound = template.source == observation.target
                && template.volumes == observation.volumes
                && template.archive.kind == AppResourceKind::File
                && !template.archive.name.is_empty()
                && !template.archive.uid.is_empty();
            if !bound {
                None // Unbound history is not authority to inspect a foreign archive.
            } else {
                match state
                    .runtime()
                    .inspect_builder_restart_archive(&template)
                    .await?
                {
                    Some(true) => return Ok(false),
                    None => {
                        return Err(anyhow!(
                            "Original archive availability is unknown; no stopped boundary or restore was submitted"
                        ));
                    }
                    Some(false) => Some(BuilderRestartArchiveUnavailable {
                        template,
                        kind: BuilderRestartArchiveUnavailableKind::PositiveUnavailable,
                    }),
                }
            }
        }
    };
    // Archive observation and failure CAS both precede the original Stopped
    // transition, closing the new crash window without a historical key gate.
    validate_guard(guard, current).await?;
    ensure!(
        state
            .runtime()
            .observe_builder_restart_stop(&observation.target)
            .await?
            == Some(BuilderRestartStopKind::CapturedContainerAbsent),
        "Original CID absence changed before old-recipe settlement"
    );
    *current = state
        .userapp_store
        .fail_unrebuildable_builder_restart(current, observation, unavailable.as_ref())
        .await?;
    Ok(true)
}

pub(super) async fn recover(state: &AppState, snapshot: &ComputeControlRecord) -> Result<()> {
    ensure!(
        eligible(snapshot),
        "Restart is not at the original Docker stop boundary"
    );
    let context = snapshot.execution_context().map_err(anyhow::Error::msg)?;
    let receipt = snapshot
        .lease
        .as_ref()
        .context("Original Docker compute lease missing")?;
    let guard = state
        .runtime()
        .resume_original_compute_operation(&context, snapshot.scope, receipt)
        .await?;
    validate_guard(guard.as_ref(), snapshot).await?;
    let mut current = snapshot.clone();
    let result: Result<()> = async {
        // eligible() requires the original pre-restore Stopping boundary. An
        // already-Stopped historical record may contain an unknown Create, so
        // it retains the existing continuation and is never terminalized here.
        if current.stage == ComputeControlStage::Stopping.as_str() {
            let target = BuilderControlTarget::from_observation_checkpoint(&current.checkpoint)?;
            // Docker mounts are HostPath witnesses; builder_start::retained_volumes
            // deliberately accepts Kubernetes PVCs only and cannot be used here.
            let volumes: Vec<AppResourceIdentity> = serde_json::from_value(current.checkpoint
                .get("builder_volumes").cloned().context("Original Docker mount witness missing")?)?;
            let kind = state.runtime().observe_builder_restart_stop(&target).await?
                .context("Original Stop is neither acknowledged nor its exact CID absent")?;
            let observation = BuilderRestartStopObservation { target, volumes, kind };
            observation.validate().map_err(anyhow::Error::msg)?;
            validate_guard(guard.as_ref(), &current).await?;
            if settle_unrebuildable_absent_recipe(state, &mut current, &observation, guard.as_ref()).await? {
                return Err(anyhow!("Original Restart recipe is unavailable; old execution is closed without starting compute"));
            }
            current = state.userapp_store.confirm_builder_restart_stopped(&current, &observation).await?;
        }
        // Keep the existing restore-before-Starting CAS and existing creator
        // observation, archive, mounts and startup behavior. Holding this real
        // original FD serializes recovery executors without adding an early
        // Starting state or a new unknown-Create dispatch protocol.
        resume_builder_start_with_guard(state, &current, Some(guard.as_ref())).await
    }.await;
    let mut actual = state
        .userapp_store
        .get_compute_control(&snapshot.app_id, &snapshot.operation_id)
        .await?
        .context("Original compute record disappeared")?;
    if let Err(error) = &result {
        // Report against the last durable original phase, not the speculative
        // local Stopped snapshot after the existing continuation has progressed.
        if actual.lifecycle_id == snapshot.lifecycle_id
            && actual.scope == snapshot.scope
            && actual.generation == snapshot.generation
            && actual.executor_id == snapshot.executor_id
            && actual.lease.as_ref() == Some(receipt)
        {
            let _recorded =
                failure::record(state, &actual, &identity(&actual)?, false, error).await?;
        }
        actual = state
            .userapp_store
            .get_compute_control(&snapshot.app_id, &snapshot.operation_id)
            .await?
            .context("Original compute record disappeared after failure reporting")?;
    }
    let cleanup_confirmed = match actual.state {
        ComputeControlState::Succeeded | ComputeControlState::Failed => true,
        ComputeControlState::Pending
        | ComputeControlState::Running
        | ComputeControlState::RecoveryRequired
        | ComputeControlState::Superseded => false,
    };
    if cleanup_confirmed && actual.lease.as_ref() == Some(receipt) {
        validate_guard(guard.as_ref(), &actual).await?;
        guard.release().await.map_err(anyhow::Error::msg)?;
        state
            .userapp_store
            .forget_compute_lease(&identity(&actual)?, receipt)
            .await?;
    }
    // A failed/unknown continuation drops only its FD lock. It does not clear
    // the original marker or claim that an unobserved Create/Start completed.
    result
}

#[cfg(test)]
mod tests;
