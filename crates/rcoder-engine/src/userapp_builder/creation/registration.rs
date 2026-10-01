//! Only a completed original replacement may retire a stale workload registry.
use super::*;

pub(super) async fn capture_predecessor(
    state: &AppState,
    operation: &UserAppOperationRecord,
) -> Result<Option<shared_types::BuilderCreationPredecessor>> {
    let context = shared_types::UserAppExecutionContext {
        app_id: operation.app_id.clone(),
        lifecycle_id: operation.lifecycle_id.clone(),
        operation_id: operation.operation_id.clone(),
        executor_id: operation
            .executor_id
            .clone()
            .ok_or_else(|| anyhow!("Builder executor missing"))?,
        request_fingerprint: operation.request_fingerprint.clone(),
    };
    let target = crate::userapp_builder::adoption::capture_bound_target(state, &context).await?;
    if target
        .workload
        .as_ref()
        .is_none_or(|workload| workload.kind != shared_types::AppResourceKind::StatefulSet)
    {
        return Ok(None);
    }
    let source = shared_types::BuilderCreationPredecessor {
        volumes: state
            .runtime()
            .capture_builder_compute_volumes(&target)
            .await?,
        target,
    };
    source
        .validate_operation(operation)
        .map_err(anyhow::Error::msg)?;
    Ok(Some(source))
}

pub(super) async fn attach_predecessor(
    state: &AppState,
    operation: &UserAppOperationRecord,
    mut evidence: shared_types::BuilderCreationEvidence,
) -> Result<shared_types::BuilderCreationEvidence> {
    let Some(value) = operation.checkpoint.get("builder_creation_predecessor") else {
        return Ok(evidence);
    };
    let source: shared_types::BuilderCreationPredecessor = serde_json::from_value(value.clone())
        .context("Decode captured builder registration predecessor")?;
    source
        .validate_operation(operation)
        .map_err(anyhow::Error::msg)?;
    if source
        .target
        .workload
        .as_ref()
        .map(|workload| &workload.uid)
        == evidence
            .target
            .workload
            .as_ref()
            .map(|workload| &workload.uid)
    {
        return Ok(evidence);
    }
    let volumes = state
        .runtime()
        .capture_builder_compute_volumes(&evidence.target)
        .await?;
    source
        .validate_replacement(&evidence.target, &volumes)
        .map_err(anyhow::Error::msg)?;
    evidence.registration_predecessor = Some(source);
    Ok(evidence)
}

pub(super) fn completed_checkpoint(
    evidence: &shared_types::BuilderCreationEvidence,
) -> Result<serde_json::Value> {
    let mut checkpoint = serde_json::to_value(&evidence.container)?;
    if evidence.registration_predecessor.is_some() {
        checkpoint
            .as_object_mut()
            .ok_or_else(|| anyhow!("Builder completion is not an object"))?
            .insert(
                "builder_creation_evidence".into(),
                serde_json::to_value(evidence)?,
            );
    }
    Ok(checkpoint)
}

pub(super) async fn register_completion(
    state: &AppState,
    operation: &UserAppOperationRecord,
    instance: &str,
    verified: &ContainerBasicInfo,
) -> Result<()> {
    let evidence: shared_types::BuilderCreationEvidence =
        if let Some(value) = operation.checkpoint.get("builder_creation_evidence") {
            serde_json::from_value(value.clone())
                .context("Decode durable builder registration completion")?
        } else {
            let replaced = crate::userapp_builder::registered_builder(state, instance)
                .and_then(|old| old.workload_uid)
                .zip(verified.workload_uid.as_ref())
                .is_some_and(|(old, current)| old != *current);
            if !replaced {
                return crate::userapp_builder::register_builder(state, instance, verified);
            }
            if repair_live_registration(state, &operation.app_id, instance, verified).await? {
                return Ok(());
            }
            let context = shared_types::UserAppExecutionContext {
                app_id: operation.app_id.clone(),
                lifecycle_id: operation.lifecycle_id.clone(),
                operation_id: operation.operation_id.clone(),
                executor_id: operation
                    .executor_id
                    .clone()
                    .ok_or_else(|| anyhow!("Original builder executor missing"))?,
                request_fingerprint: operation.request_fingerprint.clone(),
            };
            state
                .runtime()
                .recover_builder_creation(&context)
                .await?
                .ok_or_else(|| {
                    anyhow!("Original completed builder replacement has no runtime creator receipt")
                })?
        };
    evidence
        .validate_operation(operation)
        .map_err(anyhow::Error::msg)?;
    if evidence.container.container_id != verified.container_id || instance != operation.app_id {
        return Err(anyhow!(
            "Completed builder registration physical identity changed"
        ));
    }
    let live = state
        .runtime()
        .capture_bound_builder_control(
            &evidence.target.context,
            evidence.target.resource_binding.as_ref(),
        )
        .await?;
    validate_completed_resource(&evidence, &live, Some(verified.clone()))?;
    let volumes = state
        .runtime()
        .capture_builder_compute_volumes(&live)
        .await?;
    state
        .projects
        .register_completed_builder_replacement(operation, &evidence, &volumes)
        .await
        .context("Register completed UserApp builder replacement")
}

/// Before another creation is admitted, close an older acknowledged upgrade's
/// registration gap. A current name/label only locates the candidate; the
/// unique original durable operation and private creator receipt authorize it.
pub(in crate::userapp_builder) async fn repair_live_registration(
    state: &AppState,
    app_id: &str,
    instance: &str,
    verified: &ContainerBasicInfo,
) -> Result<bool> {
    let replaced = crate::userapp_builder::registered_builder(state, instance)
        .and_then(|old| old.workload_uid)
        .zip(verified.workload_uid.as_ref())
        .is_some_and(|(old, current)| old != *current);
    if !replaced {
        return Ok(false);
    }
    let app = state
        .userapp_store
        .get_application(app_id)
        .await?
        .ok_or_else(|| anyhow!("Builder registration recovery lifecycle missing"))?;
    if app.state != shared_types::UserAppLifecycleState::Active || instance != app_id {
        return Err(anyhow!(
            "Builder registration recovery lifecycle is not active"
        ));
    }
    let workload_uid = verified
        .workload_uid
        .as_deref()
        .ok_or_else(|| anyhow!("Builder workload identity missing"))?;
    let candidates = state
        .projects
        .completed_builder_registration_candidates(
            app_id,
            &app.lifecycle_id,
            &verified.container_id,
            workload_uid,
        )
        .await?;
    let mut original = None;
    for candidate in candidates {
        let context = shared_types::UserAppExecutionContext {
            app_id: candidate.app_id.clone(),
            lifecycle_id: candidate.lifecycle_id.clone(),
            operation_id: candidate.operation_id.clone(),
            executor_id: candidate
                .executor_id
                .clone()
                .ok_or_else(|| anyhow!("Original builder creator executor missing"))?,
            request_fingerprint: candidate.request_fingerprint.clone(),
        };
        let Some(receipt) = state.runtime().recover_builder_creation(&context).await? else {
            continue;
        };
        let creator = receipt
            .target
            .resource_binding
            .as_ref()
            .is_some_and(|binding| {
                binding.adopted_by_operation == candidate.operation_id
                    && binding.physical_uid == workload_uid
            });
        if !creator {
            continue;
        }
        receipt
            .validate_operation(&candidate)
            .map_err(anyhow::Error::msg)?;
        if receipt.container.container_id != verified.container_id {
            return Err(anyhow!(
                "Original builder creator receipt physical identity changed"
            ));
        }
        let evidence = if let Some(recorded) = candidate.checkpoint.get("builder_creation_evidence")
        {
            let evidence: shared_types::BuilderCreationEvidence =
                serde_json::from_value(recorded.clone())?;
            validate_completed_resource(
                &evidence,
                &receipt.target,
                Some(receipt.container.clone()),
            )?;
            evidence
        } else {
            receipt
        };
        if original.replace((candidate, evidence)).is_some() {
            return Err(anyhow!(
                "Multiple original builder replacement receipts match this resource"
            ));
        }
    }
    let Some((operation, evidence)) = original else {
        return Err(anyhow!(
            "Builder workload changed but its original succeeded replacement receipt is unavailable"
        ));
    };
    let live = state
        .runtime()
        .capture_bound_builder_control(
            &evidence.target.context,
            evidence.target.resource_binding.as_ref(),
        )
        .await?;
    validate_completed_resource(&evidence, &live, Some(verified.clone()))?;
    let volumes = state
        .runtime()
        .capture_builder_compute_volumes(&live)
        .await?;
    state
        .projects
        .register_completed_builder_replacement(&operation, &evidence, &volumes)
        .await?;
    Ok(true)
}
