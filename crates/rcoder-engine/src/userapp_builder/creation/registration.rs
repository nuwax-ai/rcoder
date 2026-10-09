//! Succeeded, identity-bound runtime confirmations repair builder registration.
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
    if operation.kind != UserAppOperationKind::EnsureBuilder
        || operation.state != UserAppOperationState::Succeeded
        || instance != operation.app_id
    {
        return Err(anyhow!(
            "Builder completion instance differs from its application"
        ));
    }
    // PG Kubernetes publication must commit the durable registry and binding
    // before updating its memory mirror, including ordinary creation/reuse.
    if requires_durable_confirmation(state, instance, verified) {
        return register_confirmations(
            state,
            &operation.app_id,
            instance,
            verified,
            Some(operation),
        )
        .await;
    }
    let Some(recorded) = operation.checkpoint.get("builder_creation_evidence") else {
        return crate::userapp_builder::register_builder(state, instance, verified);
    };
    let evidence: shared_types::BuilderCreationEvidence = serde_json::from_value(recorded.clone())
        .context("Decode durable builder registration completion")?;
    validate_confirmation(operation, &evidence, verified)?;
    publish_confirmation(state, operation, &evidence, verified, &[]).await
}

#[derive(Debug, thiserror::Error)]
#[error("Current builder requires an admitted completion confirmation")]
pub(in crate::userapp_builder) struct BuilderRegistrationConfirmationRequired;

fn requires_durable_confirmation(
    state: &AppState,
    instance: &str,
    verified: &ContainerBasicInfo,
) -> bool {
    // Ordinary completion may join control and registry rows only when their
    // configured SQL scope is shared. Independent UserApp PG storage remains
    // valid for the existing ordinary path; it must not acquire a new implicit
    // dependency on the Agent database's lifecycle tables.
    let shared_registry = state.projects.is_postgres()
        && state
            .config
            .userapp_storage
            .postgres
            .as_ref()
            .is_none_or(|control| control.shares_connection_scope(&state.config.storage.postgres));
    (shared_registry && verified.workload_uid.is_some())
        || crate::userapp_builder::registered_builder(state, instance)
            .and_then(|old| old.workload_uid)
            .zip(verified.workload_uid.as_ref())
            .is_some_and(|(old, current)| old != *current)
}

/// A current name only locates candidates. Each succeeded operation requires
/// its private completion receipt, exact operation identity and current Pod /
/// workload witnesses; no request age or "latest" heuristic grants authority.
pub(in crate::userapp_builder) async fn repair_live_registration(
    state: &AppState,
    app_id: &str,
    instance: &str,
    verified: &ContainerBasicInfo,
) -> Result<bool> {
    if !requires_durable_confirmation(state, instance, verified) {
        return Ok(false);
    }
    register_confirmations(state, app_id, instance, verified, None).await?;
    Ok(true)
}

async fn register_confirmations(
    state: &AppState,
    app_id: &str,
    instance: &str,
    verified: &ContainerBasicInfo,
    completed: Option<&UserAppOperationRecord>,
) -> Result<()> {
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
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| anyhow!("Builder workload identity missing"))?;
    let mut candidates = state
        .projects
        .completed_builder_registration_candidates(
            app_id,
            &app.lifecycle_id,
            &verified.container_id,
            workload_uid,
        )
        .await?;
    // The directly awaited completion is available to non-PG backends too.
    // PG candidates come from the same durable lifecycle store; deduplicate
    // without changing the authoritative stored record if it already exists.
    if let Some(operation) = completed
        && !candidates
            .iter()
            .any(|candidate| candidate.operation_id == operation.operation_id)
    {
        candidates.push(operation.clone());
    }
    register_candidates(state, app_id, verified, candidates).await
}

async fn register_candidates(
    state: &AppState,
    app_id: &str,
    verified: &ContainerBasicInfo,
    mut candidates: Vec<UserAppOperationRecord>,
) -> Result<()> {
    let app = state
        .userapp_store
        .get_application(app_id)
        .await?
        .ok_or_else(|| anyhow!("Builder registration lifecycle disappeared"))?;
    candidates.sort_by(|left, right| left.operation_id.cmp(&right.operation_id));
    let mut selected: Option<(
        UserAppOperationRecord,
        shared_types::BuilderCreationEvidence,
    )> = None;
    let mut predecessor: Option<shared_types::BuilderCreationPredecessor> = None;
    let mut constraints = Vec::new();
    for candidate in candidates {
        if candidate.app_id != app_id
            || candidate.lifecycle_id != app.lifecycle_id
            || candidate.kind != UserAppOperationKind::EnsureBuilder
            || candidate.state != UserAppOperationState::Succeeded
        {
            return Err(anyhow!(
                "Builder completion candidate operation identity differs"
            ));
        }
        let context = shared_types::UserAppExecutionContext {
            app_id: candidate.app_id.clone(),
            lifecycle_id: candidate.lifecycle_id.clone(),
            operation_id: candidate.operation_id.clone(),
            executor_id: candidate
                .executor_id
                .clone()
                .ok_or_else(|| anyhow!("Builder completion executor missing"))?,
            request_fingerprint: candidate.request_fingerprint.clone(),
        };
        context
            .validate_identity(app_id)
            .map_err(anyhow::Error::msg)?;
        // Durable predecessor constraints survive receipt cleanup / Service
        // overwrite. Validate them before deciding whether this candidate has
        // a private confirmation available for registration authority.
        let recorded = candidate
            .checkpoint
            .get("builder_creation_evidence")
            .map(|value| {
                serde_json::from_value::<shared_types::BuilderCreationEvidence>(value.clone())
            })
            .transpose()
            .context("Decode recorded builder completion evidence")?;
        if let Some(evidence) = &recorded {
            validate_confirmation(&candidate, evidence, verified)?;
            check_predecessor_consistency(&mut predecessor, evidence)?;
            constraints.push((candidate.clone(), evidence.clone()));
        } else {
            validate_checkpoint(&candidate, verified)?;
        }
        let Some(receipt) = state.runtime().recover_builder_creation(&context).await? else {
            // The mutable Service confirmation can have been superseded by a
            // later successful ensure. Another independently validated private
            // receipt for the same current resource can close the registry gap.
            continue;
        };
        validate_confirmation(&candidate, &receipt, verified)?;
        let evidence = if let Some(evidence) = recorded {
            validate_same_resource(&evidence.target, &receipt.target)?;
            if evidence.target.resource_binding != receipt.target.resource_binding {
                return Err(anyhow!(
                    "Recorded builder binding differs from its private completion"
                ));
            }
            evidence
        } else {
            receipt
        };
        check_predecessor_consistency(&mut predecessor, &evidence)?;
        constraints.push((candidate.clone(), evidence.clone()));
        if let Some((_, previous)) = &selected {
            validate_same_resource(&previous.target, &evidence.target)?;
            if previous
                .target
                .resource_binding
                .as_ref()
                .zip(evidence.target.resource_binding.as_ref())
                .is_some_and(|(left, right)| left != right)
            {
                return Err(anyhow!(
                    "Builder completion receipts disagree on canonical binding"
                ));
            }
        } else {
            selected = Some((candidate, evidence));
        }
    }
    let Some((operation, evidence)) = selected else {
        if let Some((_, evidence)) = constraints.first() {
            inspect_confirmation_constraints(state, evidence, verified, &constraints).await?;
        }
        return Err(BuilderRegistrationConfirmationRequired.into());
    };
    publish_confirmation(state, &operation, &evidence, verified, &constraints).await
}

fn check_predecessor_consistency(
    previous: &mut Option<shared_types::BuilderCreationPredecessor>,
    evidence: &shared_types::BuilderCreationEvidence,
) -> Result<()> {
    if let Some(source) = &evidence.registration_predecessor {
        if let Some(previous) = previous.as_ref() {
            validate_same_resource(&previous.target, &source.target)?;
            if volume_identities(&previous.volumes) != volume_identities(&source.volumes) {
                return Err(anyhow!(
                    "Builder completion predecessors disagree on workspace identity"
                ));
            }
        } else {
            *previous = Some(source.clone());
        }
    }
    Ok(())
}

fn volume_identities(
    volumes: &[shared_types::AppResourceIdentity],
) -> std::collections::BTreeSet<(&str, &str)> {
    volumes
        .iter()
        .map(|volume| (volume.name.as_str(), volume.uid.as_str()))
        .collect()
}

fn validate_same_resource(
    left: &shared_types::BuilderControlTarget,
    right: &shared_types::BuilderControlTarget,
) -> Result<()> {
    let workload = |target: &shared_types::BuilderControlTarget| {
        target
            .workload
            .as_ref()
            .map(|resource| (resource.kind, resource.name.clone(), resource.uid.clone()))
    };
    let pod = |target: &shared_types::BuilderControlTarget| {
        target
            .pod
            .as_ref()
            .map(|resource| (resource.name.clone(), resource.uid.clone()))
    };
    if left.context.app_id != right.context.app_id
        || left.context.lifecycle_id != right.context.lifecycle_id
        || workload(left) != workload(right)
        || pod(left) != pod(right)
    {
        return Err(anyhow!("Builder completion resource identity differs"));
    }
    Ok(())
}

fn validate_checkpoint(
    operation: &UserAppOperationRecord,
    verified: &ContainerBasicInfo,
) -> Result<()> {
    let checkpoint: ContainerBasicInfo = serde_json::from_value(operation.checkpoint.clone())
        .context("Decode succeeded builder registration identity")?;
    if checkpoint.container_id != verified.container_id
        || checkpoint
            .workload_uid
            .as_ref()
            .is_some_and(|uid| verified.workload_uid.as_ref() != Some(uid))
        || checkpoint.container_name != verified.container_name
        || checkpoint.project_id != operation.app_id
        || checkpoint.created_at != verified.created_at
        || checkpoint.container_name.is_empty()
    {
        return Err(anyhow!(
            "Builder completion physical identity differs from checkpoint or runtime"
        ));
    }
    Ok(())
}

fn validate_confirmation(
    operation: &UserAppOperationRecord,
    evidence: &shared_types::BuilderCreationEvidence,
    verified: &ContainerBasicInfo,
) -> Result<()> {
    evidence
        .validate_operation(operation)
        .map_err(anyhow::Error::msg)?;
    if operation.state != UserAppOperationState::Succeeded {
        return Err(anyhow!("Builder registration operation is not succeeded"));
    }
    validate_checkpoint(operation, verified)?;
    if evidence.target.workload.as_ref().is_some_and(|workload| {
        workload.kind == shared_types::AppResourceKind::StatefulSet
            && (verified.workload_uid.as_deref() != Some(workload.uid.as_str())
                || evidence.container.container_name != workload.name)
    }) || verified.container_id != evidence.container.container_id
        || evidence
            .container
            .workload_uid
            .as_ref()
            .is_some_and(|uid| verified.workload_uid.as_ref() != Some(uid))
        || verified.container_name != evidence.container.container_name
        || verified.project_id != operation.app_id
        || evidence.container.project_id != operation.app_id
        || verified.created_at != evidence.container.created_at
    {
        return Err(anyhow!(
            "Builder completion private resource identity differs from runtime"
        ));
    }
    Ok(())
}

async fn inspect_confirmation_constraints(
    state: &AppState,
    evidence: &shared_types::BuilderCreationEvidence,
    verified: &ContainerBasicInfo,
    constraints: &[(
        UserAppOperationRecord,
        shared_types::BuilderCreationEvidence,
    )],
) -> Result<Vec<shared_types::AppResourceIdentity>> {
    let live = state
        .runtime()
        .capture_bound_builder_control(
            &evidence.target.context,
            evidence.target.resource_binding.as_ref(),
        )
        .await?;
    validate_same_resource(&evidence.target, &live)?;
    validate_completed_resource(evidence, &live, Some(verified.clone()))?;
    let volumes = state
        .runtime()
        .capture_builder_compute_volumes(&live)
        .await?;
    for (candidate, confirmation) in constraints {
        validate_same_resource(&confirmation.target, &live)?;
        confirmation
            .validate_registration_replacement(candidate, &volumes)
            .map_err(anyhow::Error::msg)?;
    }
    Ok(volumes)
}

async fn publish_confirmation(
    state: &AppState,
    operation: &UserAppOperationRecord,
    evidence: &shared_types::BuilderCreationEvidence,
    verified: &ContainerBasicInfo,
    constraints: &[(
        UserAppOperationRecord,
        shared_types::BuilderCreationEvidence,
    )],
) -> Result<()> {
    let volumes = inspect_confirmation_constraints(state, evidence, verified, constraints).await?;
    // Endpoint fields may legitimately change after the private confirmation;
    // storage uses the receipt's identity, not its historical network address.
    let mut refreshed = evidence.clone();
    refreshed.container = verified.clone();
    state
        .projects
        .register_completed_builder_replacement(operation, &refreshed, &volumes)
        .await
        .context("Register completed UserApp builder confirmation")
}

#[cfg(test)]
#[path = "registration_tests.rs"]
mod tests;
