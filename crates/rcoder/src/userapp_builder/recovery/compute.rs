//! Reclaim only a confirmed terminal compute control's exact physical receipt.
//! Pending/uncertain/superseded execution is never inferred safe from age.
use super::*;

pub(super) async fn discover_compute_leases(
    store: std::sync::Arc<dyn shared_types::UserAppLifecycleStore>,
    runtime: std::sync::Arc<dyn container_runtime_api::UserAppDeploymentRuntime>,
    tasks: &mut RecoveryTasks,
    cursor: &mut Option<String>,
) -> anyhow::Result<()> {
    if tasks.is_full() {
        return Ok(());
    }
    let query_cursor = cursor.clone();
    let read = tokio::time::timeout(
        SCAN_READ_TIMEOUT,
        store.scan_compute_controls(query_cursor.as_deref(), SCAN_PAGE_SIZE),
    );
    tokio::pin!(read);
    let page = loop {
        tokio::select! {
            result = &mut read => break result.map_err(|_| anyhow::anyhow!("Compute control scan timed out"))??,
            result = tasks.next(), if !tasks.active.is_empty() => {
                if let Some((operation_id, Err(error))) = result {
                    tracing::warn!(%error, %operation_id, "Recovery task failed during compute scan");
                }
            }
        }
    };
    if page.is_empty() {
        *cursor = None;
        return Ok(());
    }
    for record in page {
        if tasks.is_full() {
            break;
        }
        *cursor = Some(record.operation_id.clone());
        if !matches!(
            record.state,
            shared_types::ComputeControlState::Succeeded
                | shared_types::ComputeControlState::Failed
        ) {
            continue;
        }
        if record.state == shared_types::ComputeControlState::Succeeded
            && let Some(value) = record.checkpoint.get("builder_restart_template")
        {
            let template: shared_types::BuilderRestartTemplate =
                match serde_json::from_value(value.clone()) {
                    Ok(template) => template,
                    Err(error) => {
                        tracing::error!(%error, operation_id = %record.operation_id,
                            "Invalid restart archive checkpoint; retaining its lease and archive");
                        continue;
                    }
                };
            if template.source.context.app_id != record.app_id
                || template.source.context.lifecycle_id != record.lifecycle_id
                || template.source.context.operation_id != record.operation_id
            {
                tracing::error!(operation_id = %record.operation_id,
                    "Restart archive checkpoint belongs to another operation; retaining its lease and archive");
                continue;
            }
            let store = store.clone();
            let runtime = runtime.clone();
            tasks.push(format!("compute:{}", record.operation_id), async move {
                let context = record.execution_context().map_err(anyhow::Error::msg)?;
                if let Some(receipt) = &record.lease {
                    runtime
                        .release_app_operation_receipt(&context, receipt)
                        .await?;
                    let identity = shared_types::ComputeExecutorIdentity {
                        app_id: record.app_id.clone(),
                        lifecycle_id: record.lifecycle_id.clone(),
                        scope: record.scope,
                        operation_id: record.operation_id.clone(),
                        generation: record.generation,
                        executor_id: context.executor_id.clone(),
                    };
                    store.forget_compute_lease(&identity, receipt).await?;
                }
                runtime.cleanup_builder_restart_archive(&template).await?;
                let current = store
                    .get_compute_control(&record.app_id, &record.operation_id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("Archive cleanup operation disappeared"))?;
                anyhow::ensure!(
                    current.checkpoint.get("builder_restart_template")
                        == record.checkpoint.get("builder_restart_template"),
                    "Archive cleanup checkpoint changed"
                );
                store.mark_restart_archive_cleaned(&current).await?;
                Ok(())
            });
            continue;
        }
        let Some(receipt) = record.lease.clone() else {
            continue;
        };
        let context = record.execution_context().map_err(anyhow::Error::msg)?;
        let identity = shared_types::ComputeExecutorIdentity {
            app_id: record.app_id,
            lifecycle_id: record.lifecycle_id,
            scope: record.scope,
            operation_id: record.operation_id,
            generation: record.generation,
            executor_id: context.executor_id.clone(),
        };
        let store = store.clone();
        let runtime = runtime.clone();
        tasks.push(format!("compute:{}", identity.operation_id), async move {
            runtime
                .release_app_operation_receipt(&context, &receipt)
                .await?;
            store.forget_compute_lease(&identity, &receipt).await?;
            Ok(())
        });
    }
    Ok(())
}

/// Committed admission survives an HTTP disconnect or process exit before spawn.
/// Only unclaimed operations may be picked up without runtime recovery evidence.
pub(super) async fn discover_pending(
    state: &std::sync::Arc<AppState>,
    tasks: &mut RecoveryTasks,
    cursor: &mut Option<String>,
) -> anyhow::Result<()> {
    if tasks.is_full() {
        return Ok(());
    }
    let page = tokio::time::timeout(
        SCAN_READ_TIMEOUT,
        state
            .userapp_store
            .scan_compute_controls(cursor.as_deref(), SCAN_PAGE_SIZE),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Pending compute scan timed out"))??;
    if page.is_empty() {
        *cursor = None;
        return Ok(());
    }
    for record in page {
        if tasks.is_full() {
            break;
        }
        *cursor = Some(record.operation_id.clone());
        if record.state == shared_types::ComputeControlState::Superseded
            && record.action == shared_types::ComputeControlAction::Restart
            && record.lease.is_some()
            && (matches!(record.stage.as_str(), "stopped" | "verifying")
                || (matches!(record.stage.as_str(), "starting" | "stopping")
                    && (record.has_conditional_compute_write()
                        || record.has_docker_compute_target())))
        {
            let flight = state.userapp_op_flight.guard()?;
            let state = state.clone();
            tasks.push(
                format!("compute-superseded:{}", record.operation_id),
                async move {
                    let _flight = flight;
                    match crate::userapp_builder::compute_control::reconcile_superseded(
                        &state, &record,
                    )
                    .await
                    {
                        Ok(_) => Ok(()),
                        Err(error)
                            if matches!(
                                error.downcast_ref::<shared_types::UserAppStoreError>(),
                                Some(shared_types::UserAppStoreError::VersionConflict)
                            ) =>
                        {
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                },
            );
            continue;
        }
        if record.state == shared_types::ComputeControlState::RecoveryRequired
            && record.stage == "draining_previous"
            && record.lease.is_none()
            && record.checkpoint.is_null()
        {
            let flight = state.userapp_op_flight.guard()?;
            let state = state.clone();
            tasks.push(
                format!("compute-drain:{}", record.operation_id),
                async move {
                    let _flight = flight;
                    resume_after_drain(&state, &record).await
                },
            );
            continue;
        }
        if record.action == shared_types::ComputeControlAction::Restart
            && (record.stage == "stopped"
                || (record.stage == "starting" && record.has_conditional_compute_write()))
            && matches!(
                record.state,
                shared_types::ComputeControlState::Running
                    | shared_types::ComputeControlState::RecoveryRequired
            )
        {
            let flight = state.userapp_op_flight.guard()?;
            let state = state.clone();
            tasks.push(
                format!("compute-resume:{}", record.operation_id),
                async move {
                    let _flight = flight;
                    crate::userapp_builder::compute_control::resume_restart_start(&state, &record)
                        .await
                },
            );
            continue;
        }
        if matches!(
            record.state,
            shared_types::ComputeControlState::Running
                | shared_types::ComputeControlState::RecoveryRequired
        ) && (matches!(
            (record.action, record.stage.as_str()),
            (
                shared_types::ComputeControlAction::Stop,
                "stopped" | "stopping"
            ) | (
                shared_types::ComputeControlAction::Restart,
                "verifying" | "starting"
            )
        ) || (record.scope == shared_types::UserAppOperationScope::Prod
            && matches!(
                (record.action, record.stage.as_str()),
                (shared_types::ComputeControlAction::Stop, "stopping")
                    | (shared_types::ComputeControlAction::Restart, "starting")
            )))
        {
            let state = state.clone();
            tasks.push(
                format!("compute-complete:{}", record.operation_id),
                async move {
                    // Only observation and an exact terminal CAS; cancellation cannot
                    // leave a new container mutation in flight.
                    tokio::time::timeout(
                        Duration::from_secs(30),
                        crate::userapp_builder::compute_control::recover_confirmed(&state, &record),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("Compute completion observation timed out"))??;
                    Ok(())
                },
            );
            continue;
        }
        if record.state != shared_types::ComputeControlState::Pending {
            continue;
        }
        let flight = state.userapp_op_flight.guard()?;
        let state = state.clone();
        tasks.push(
            format!("compute-execute:{}", record.operation_id),
            async move {
                let _flight = flight;
                crate::userapp_builder::compute_control::execute_pending(&state, record).await
            },
        );
    }
    Ok(())
}

/// This preliminary read only avoids repeatedly occupying a worker for the
/// entire drain deadline. Resume still CASes the full original snapshot; the
/// executor and storage recheck drain/authority before any compute mutation.
async fn resume_after_drain(
    state: &std::sync::Arc<AppState>,
    record: &shared_types::ComputeControlRecord,
) -> anyhow::Result<()> {
    use shared_types::{ComputeControlState, UserAppStoreError};
    for id in &record.interrupted_operations {
        if let Some(old) = state
            .userapp_store
            .get_operation(&record.app_id, id)
            .await?
        {
            anyhow::ensure!(
                old.lifecycle_id == record.lifecycle_id,
                "Interrupted business lifecycle differs from compute control"
            );
            if !old.state.is_terminal() {
                // A late receipt can arrive after the original drain budget.
                // Resume the same control so its elected executor finalizes
                // the interrupted operation and releases the original lease.
                if old.kind.ends_lifecycle()
                    || !shared_types::userapp_operation_has_drain_evidence(&old)
                {
                    return Ok(());
                }
            } else if state
                .userapp_store
                .get_operation_lease(&record.app_id, id)
                .await?
                .is_some()
            {
                return Ok(());
            }
        } else if let Some(old) = state
            .userapp_store
            .get_compute_control(&record.app_id, id)
            .await?
        {
            anyhow::ensure!(
                old.lifecycle_id == record.lifecycle_id,
                "Interrupted compute lifecycle differs from current control"
            );
            let unclaimed = old.state == ComputeControlState::Superseded
                && old.executor_id.is_none()
                && old.stage == "accepted";
            if !(matches!(
                old.state,
                ComputeControlState::Succeeded | ComputeControlState::Failed
            ) || unclaimed)
                || old.lease.is_some()
            {
                return Ok(());
            }
        } else {
            anyhow::bail!("Interrupted operation evidence missing: {id}");
        }
    }
    let pending = match state.userapp_store.resume_compute_drain(record).await {
        Ok(pending) => pending,
        Err(UserAppStoreError::VersionConflict) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    crate::userapp_builder::compute_control::execute_pending(state, pending).await
}
