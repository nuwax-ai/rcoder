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
        // Terminal physical authority is independent of an old private archive.
        // Retain malformed/foreign archive bytes for diagnosis, while the exact
        // confirmed terminal lease still has its normal release/forget path.
        let template = if record.state == shared_types::ComputeControlState::Succeeded {
            record.checkpoint.get("builder_restart_template").and_then(|value| {
                match serde_json::from_value::<shared_types::BuilderRestartTemplate>(value.clone()) {
                    Ok(template) if template.source.context.app_id == record.app_id
                        && template.source.context.lifecycle_id == record.lifecycle_id
                        && template.source.context.operation_id == record.operation_id => Some(template),
                    Ok(_) => {
                        tracing::warn!(operation_id = %record.operation_id,
                            "Retaining foreign historical restart archive; terminal lease cleanup remains available");
                        None
                    }
                    Err(error) => {
                        tracing::warn!(%error, operation_id = %record.operation_id,
                            "Retaining undecodable historical restart archive; terminal lease cleanup remains available");
                        None
                    }
                }
            })
        } else {
            None
        };
        if record.lease.is_none() && template.is_none() {
            continue;
        }
        let store = store.clone();
        let runtime = runtime.clone();
        tasks.push(format!("compute:{}", record.operation_id), async move {
            with_stall_budget("Clean terminal compute receipts", async move {
                let context = record.execution_context().map_err(anyhow::Error::msg)?;
                if let Some(receipt) = &record.lease {
                    runtime
                        .release_app_operation_receipt(&context, receipt)
                        .await?;
                    store
                        .forget_compute_lease(
                            &shared_types::ComputeExecutorIdentity {
                                app_id: record.app_id.clone(),
                                lifecycle_id: record.lifecycle_id.clone(),
                                scope: record.scope,
                                operation_id: record.operation_id.clone(),
                                generation: record.generation,
                                executor_id: context.executor_id.clone(),
                            },
                            receipt,
                        )
                        .await?;
                }
                if let Some(template) = template {
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
                }
                // These per-operation receipts no longer grant execution authority.
                // A malformed archive above was not edited, deleted or marked clean.
                runtime.cleanup_compute_receipt_files(&context).await?;
                Ok(())
            })
            .await?
        });
    }
    Ok(())
}

/// Reclaim builder creation/cancellation receipts whose operation record is
/// terminal. The receipts themselves drive the iteration, so a completed
/// cleanup never reappears; missing or unfinished records keep their evidence.
pub(super) async fn sweep_builder_creation_receipts(
    store: std::sync::Arc<dyn shared_types::UserAppLifecycleStore>,
    runtime: std::sync::Arc<dyn container_runtime_api::UserAppDeploymentRuntime>,
    tasks: &mut RecoveryTasks,
) -> anyhow::Result<()> {
    if tasks.is_full() {
        return Ok(());
    }
    let contexts = tokio::time::timeout(
        SCAN_READ_TIMEOUT,
        runtime.list_builder_creation_receipt_contexts(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Builder creation receipt inventory timed out"))??;
    for context in contexts {
        if tasks.is_full() {
            break;
        }
        let Some(record) = tokio::time::timeout(
            SCAN_READ_TIMEOUT,
            store.get_operation(&context.app_id, &context.operation_id),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Builder creation receipt operation read timed out"))??
        else {
            // No record to confirm a terminal outcome — retain the receipts.
            continue;
        };
        if !record.state.is_terminal() {
            continue;
        }
        let runtime = runtime.clone();
        tasks.push(
            format!("creation-receipts:{}", context.operation_id),
            async move {
                with_stall_budget("Clean terminal builder creation receipts", async move {
                    runtime.cleanup_builder_creation_receipts(&context).await?;
                    anyhow::Ok(())
                })
                .await?
            },
        );
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
        if record.can_observe_superseded_compute_drain() {
            let flight = state.userapp_op_flight.guard()?;
            let state = state.clone();
            tasks.push(
                format!("compute-superseded-drain:{}", record.operation_id),
                async move {
                    let _flight = flight;
                    tokio::time::timeout(
                        Duration::from_secs(30),
                        crate::userapp_builder::compute_drain_recovery::recover_superseded_early(
                            &state, &record,
                        ),
                    )
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("Superseded early compute observation timed out")
                    })??;
                    Ok(())
                },
            );
            continue;
        }
        if record.state == shared_types::ComputeControlState::Superseded
            && record.action == shared_types::ComputeControlAction::Restart
            && record.lease.is_some()
            && (matches!(record.stage.as_str(), "stopped" | "verifying")
                || (matches!(record.stage.as_str(), "starting" | "stopping")
                    && (record.has_conditional_compute_write()
                        || record.has_docker_compute_target()
                        || crate::userapp_builder::compute_control::missing_builder_creation(
                            &record,
                        ))))
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
        // Total-deadline settlement is dispatched as a CONCURRENT observer,
        // never an exclusive scheduler: a record whose lease is still renewed
        // (a takeover continuation mid-flight, or an executor that legitimately
        // outran the budget) keeps receiving its normal recovery branches below,
        // while the settlement itself defers until the lease provably stops
        // renewing. Any CAS race between the two resolves by revision — the
        // settlement maps its loss to a benign deferral. The dispatch reserves
        // one further slot: with a single slot left the cheap, retrying
        // observer steps aside so a saturated scan never starves the record's
        // real continuation branch.
        if matches!(
            record.state,
            shared_types::ComputeControlState::Running
                | shared_types::ComputeControlState::RecoveryRequired
        ) && tasks.remaining_capacity() >= 2
            && crate::userapp_builder::compute_control::compute_deadline_expired(state, &record)
                .await
        {
            let state = state.clone();
            let settled = record.clone();
            tasks.push(
                format!("compute-deadline:{}", record.operation_id),
                async move {
                    tokio::time::timeout(
                        Duration::from_secs(30),
                        crate::userapp_builder::compute_control::settle_expired_compute_control(
                            &state, &settled,
                        ),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("Compute deadline settlement timed out"))??;
                    Ok(())
                },
            );
        }
        if crate::userapp_builder::compute_control::can_recover_original_docker_stop(&record) {
            let flight = state.userapp_op_flight.guard()?;
            let state = state.clone();
            tasks.push(
                format!("compute-original-docker-stop:{}", record.operation_id),
                async move {
                    let _flight = flight;
                    // Bound scheduling exactly like the existing compute-resume
                    // branch. Its Docker/readiness deadlines remain unchanged;
                    // a new 60s whole-worker timeout would cancel legitimate
                    // restore/Start before those original budgets expire.
                    crate::userapp_builder::compute_control::recover_original_docker_stop(
                        &state, &record,
                    )
                    .await
                },
            );
            continue;
        }
        if crate::userapp_builder::compute_control::can_confirm_kubernetes_restart_stop(&record) {
            let state = state.clone();
            tasks.push(
                format!("compute-restart-stop-receipt:{}", record.operation_id),
                async move {
                    // Observation plus an exact stage CAS; no physical mutation
                    // is re-issued, so no operation flight is held.
                    tokio::time::timeout(
                        Duration::from_secs(30),
                        crate::userapp_builder::compute_control::confirm_kubernetes_restart_stop(
                            &state, &record,
                        ),
                    )
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("Kubernetes restart stop confirmation timed out")
                    })??;
                    Ok(())
                },
            );
            continue;
        }
        if record.can_recover_compute_drain() {
            let flight = state.userapp_op_flight.guard()?;
            let state = state.clone();
            tasks.push(
                format!("compute-drain:{}", record.operation_id),
                async move {
                    let _flight = flight;
                    crate::userapp_builder::compute_drain_recovery::recover_and_execute(
                        &state, &record,
                    )
                    .await
                },
            );
            continue;
        }
        if record.action == shared_types::ComputeControlAction::Restart
            && (record.stage == "stopped"
                || (record.stage == "starting"
                    && (record.has_conditional_compute_write()
                        || crate::userapp_builder::compute_control::missing_builder_creation(
                            &record,
                        ))))
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

#[cfg(all(test, feature = "userapp-turso"))]
mod tests;
