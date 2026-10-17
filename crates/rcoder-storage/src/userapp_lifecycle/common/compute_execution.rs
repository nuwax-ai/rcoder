//! Compute execution ledger. Transactions contain no runtime I/O.
use super::{compute, repo};
use crate::{db::schema::Backend, userapp_lifecycle::storage};
use shared_types::*;
use toasty::Executor;
use toasty_core::schema::db::Type;
type Error = UserAppStoreError;
fn invalid(message: &str) -> Error {
    Error::InvalidOperation(message.into())
}

#[path = "compute_restart_absence.rs"]
mod restart_absence;
use restart_absence::{
    retained_builder_creation, retained_builder_volume_evidence, stopped_builder_absence,
};

fn owns(record: &ComputeControlRecord, identity: &ComputeExecutorIdentity) -> bool {
    record.app_id == identity.app_id
        && record.lifecycle_id == identity.lifecycle_id
        && record.scope == identity.scope
        && record.operation_id == identity.operation_id
        && record.generation == identity.generation
        && record.executor_id.as_deref() == Some(identity.executor_id.as_str())
}
fn validate_receipt(
    identity: &ComputeExecutorIdentity,
    receipt: &UserAppOperationLeaseReceipt,
) -> Result<(), Error> {
    receipt.validate().map_err(Error::InvalidOperation)?;
    let family = match identity.scope {
        UserAppOperationScope::Dev => ServiceType::UserappBuilder,
        UserAppOperationScope::Prod => ServiceType::Userapp,
        UserAppOperationScope::Application => return Err(invalid("Invalid compute scope")),
    };
    if receipt.service_type() != &family {
        return Err(invalid("Compute lease scope differs"));
    }
    Ok(())
}
pub(super) async fn bind(
    tx: &mut dyn Executor,
    backend: Backend,
    identity: &ComputeExecutorIdentity,
    receipt: &UserAppOperationLeaseReceipt,
) -> Result<ComputeControlRecord, Error> {
    validate_receipt(identity, receipt)?;
    repo::claim_app(tx, backend, &identity.app_id).await?;
    compute::check(tx, identity).await?;
    let record = compute::current(tx, identity).await?;
    if let Some(previous) = &record.lease {
        return if previous == receipt {
            Ok(record)
        } else {
            Err(invalid("Compute lease is immutable"))
        };
    }
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET lease_json=$2,revision=revision+1,updated_at_us=$3 WHERE operation_id=$1 AND revision=$4 AND lease_json IS NULL"))
        .bind(&identity.operation_id).bind(serde_json::to_string(receipt).map_err(storage)?)
        .bind(chrono::Utc::now().timestamp_micros()).bind(record.revision).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    compute::get(tx, &identity.app_id, &identity.operation_id)
        .await?
        .ok_or(Error::NotFound)
}

/// Drain proof includes durable old-operation completion AND lease release.
/// Ordinary checkpoint observation alone cannot authorize the next physical write.
pub(super) async fn drained(
    tx: &mut dyn Executor,
    record: &ComputeControlRecord,
) -> Result<(), Error> {
    for id in &record.interrupted_operations {
        if let Some(old) = repo::operation(tx, &record.app_id, id).await? {
            if old.lifecycle_id != record.lifecycle_id || !old.state.is_terminal() {
                return Err(invalid("Interrupted business operation is not reconciled"));
            }
            if crate::db::models::OperationLease::filter_by_operation_id(id)
                .first()
                .exec(tx)
                .await
                .map_err(storage)?
                .is_some()
            {
                return Err(invalid("Interrupted business lease is not released"));
            }
        } else if let Some(old) = compute::get(tx, &record.app_id, id).await? {
            // A superseded controller may still have a request in flight.
            let never_claimed = old.state == ComputeControlState::Superseded
                && old.executor_id.is_none()
                && old.stage == "accepted";
            if old.lifecycle_id != record.lifecycle_id
                || !(matches!(
                    old.state,
                    ComputeControlState::Succeeded | ComputeControlState::Failed
                ) || never_claimed)
                || old.lease.is_some()
            {
                return Err(invalid("Interrupted compute operation is not reconciled"));
            }
        } else {
            return Err(invalid("Interrupted operation evidence is missing"));
        }
    }
    Ok(())
}

pub(super) async fn advance(
    tx: &mut dyn Executor,
    backend: Backend,
    progress: &ComputeControlProgress,
) -> Result<ComputeControlRecord, Error> {
    let identity = &progress.identity;
    repo::claim_app(tx, backend, &identity.app_id).await?;
    compute::check(tx, identity).await?;
    let record = compute::current(tx, identity).await?;
    if record.revision != progress.expected_revision {
        return Err(Error::VersionConflict);
    }
    let state = progress
        .state
        .as_progress_storage_value()
        .ok_or_else(|| invalid(ComputeProgressPolicyError::InvalidState.message()))?;
    let stage = progress.stage.as_str();
    validate_compute_progress_transition(
        record.action,
        ComputeControlStage::from_storage_value(&record.stage),
        progress.state,
        progress.stage,
        ComputeDiagnosticValue::from_option(progress.error_code.as_deref()),
        ComputeDiagnosticValue::from_option(progress.error_message.as_deref()),
        record.lease.is_some(),
    )
    .map_err(|error| invalid(error.message()))?;
    if progress.stage != ComputeControlStage::DrainingPrevious {
        drained(tx, &record).await?;
        validate_compute_mutation_evidence(
            progress.stage,
            record.lease.is_some(),
            progress.checkpoint.is_object(),
        )
        .map_err(|error| invalid(error.message()))?;
    }
    if record.scope == UserAppOperationScope::Dev
        && record.action == ComputeControlAction::Restart
        && record.checkpoint.get("context").is_some()
    {
        // A verified result can commit its unchanged historical checkpoint.
        // This terminal-only path cannot prepare or submit another mutation.
        let terminal_observation = ComputeControlStage::from_storage_value(&record.stage)
            == Some(ComputeControlStage::Verifying)
            && progress.stage == ComputeControlStage::Completed
            && progress.state == ComputeControlState::Succeeded
            && record.checkpoint == progress.checkpoint;
        let read_target = |checkpoint: &serde_json::Value| {
            if terminal_observation {
                BuilderControlTarget::from_observation_checkpoint(checkpoint)
            } else {
                serde_json::from_value(checkpoint.clone())
            }
        };
        let original = read_target(&record.checkpoint).map_err(storage)?;
        let next = read_target(&progress.checkpoint).map_err(storage)?;
        let confirmed_absence = progress.stage == ComputeControlStage::Starting
            && stopped_builder_absence(&record, &next)
            && record.checkpoint.get("builder_volumes")
                == progress.checkpoint.get("builder_volumes");
        if original.workload.is_some() && next.workload.is_none() && !confirmed_absence {
            return Err(invalid(
                "Builder recreation requires a stopped retained-volume boundary",
            ));
        }
        if original.restart_runtime_workspace != next.restart_runtime_workspace
            && !confirmed_absence
        {
            return Err(invalid("Builder restart workspace intent is immutable"));
        }
    }
    let now = chrono::Utc::now().timestamp_micros();
    let terminal = if progress.state.is_terminal() {
        Some(now)
    } else {
        None
    };
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state=$2,stage=$3,checkpoint_json=$4,error_code=$5,error_message=$6,revision=revision+1,updated_at_us=$7,terminal_at_us=$8 WHERE operation_id=$1 AND revision=$9 AND executor_id=$10 AND state='running'"))
        .bind(&identity.operation_id).bind(state).bind(stage).bind(serde_json::to_string(&progress.checkpoint).map_err(storage)?)
        .bind_typed(progress.error_code.clone(), Type::Text).bind_typed(progress.error_message.clone(), Type::Text).bind(now).bind_typed(terminal, Type::Integer(8))
        .bind(record.revision).bind(&identity.executor_id).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    if progress.state == ComputeControlState::Succeeded
        && record.action == ComputeControlAction::Restart
        && identity.scope == UserAppOperationScope::Prod
    {
        let mut app = repo::app(tx, &identity.app_id)
            .await?
            .ok_or(Error::NotFound)?;
        let previous_revision = app.metadata_revision;
        app.runtime_policy.wake_on_traffic = Some(true);
        app.metadata_revision = previous_revision
            .checked_add(1)
            .ok_or_else(|| invalid("Metadata revision exhausted"))?;
        repo::save_app(tx, backend, &app, &identity.lifecycle_id, previous_revision).await?;
    }
    compute::get(tx, &identity.app_id, &identity.operation_id)
        .await?
        .ok_or(Error::NotFound)
}

pub(super) async fn forget(
    tx: &mut dyn Executor,
    backend: Backend,
    identity: &ComputeExecutorIdentity,
    receipt: &UserAppOperationLeaseReceipt,
) -> Result<(), Error> {
    validate_receipt(identity, receipt)?;
    repo::claim_app(tx, backend, &identity.app_id).await?;
    // Historical receipt cleanup deliberately does not require the current head;
    // it can never update the intent or release a replacement operation's lease.
    let record = compute::get(tx, &identity.app_id, &identity.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if !owns(&record, identity)
        || !matches!(
            record.state,
            ComputeControlState::Succeeded | ComputeControlState::Failed
        )
    {
        return Err(Error::VersionConflict);
    }
    let Some(stored) = record.lease else {
        return Ok(());
    };
    if stored != *receipt {
        return Err(Error::VersionConflict);
    }
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET lease_json=NULL,revision=revision+1,updated_at_us=$3 WHERE operation_id=$1 AND revision=$2"))
        .bind(&identity.operation_id).bind(record.revision).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    Ok(())
}

pub(super) async fn acknowledge_drain(
    tx: &mut dyn Executor,
    backend: Backend,
    ack: &ComputeControlDrainAcknowledgement,
) -> Result<ComputeControlRecord, Error> {
    let identity = &ack.identity;
    repo::claim_app(tx, backend, &identity.app_id).await?;
    let record = compute::get(tx, &identity.app_id, &identity.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if !owns(&record, identity)
        || record.revision != ack.expected_revision
        || record.state != ComputeControlState::Superseded
    {
        return Err(Error::VersionConflict);
    }
    match &ack.evidence {
        ComputeControlDrainEvidence::PersistedRuntimeAcknowledgement { checkpoint }
            if record.action == ComputeControlAction::Restart
                && matches!(record.stage.as_str(), "starting" | "stopping")
                && record.lease.is_some()
                && *checkpoint == record.checkpoint
                && (record.has_docker_compute_target()
                    || (record.stage == "starting" && retained_builder_creation(&record))) => {}
        ComputeControlDrainEvidence::NoMutationSubmitted if record.stage == "draining_previous" => {
        }
        ComputeControlDrainEvidence::MutationCompleted { checkpoint }
            if record.lease.is_some() && checkpoint.is_object() => {}
        ComputeControlDrainEvidence::ConditionalStartupFenced { checkpoint }
        | ComputeControlDrainEvidence::ConditionalStopFenced { checkpoint }
            if record.action == ComputeControlAction::Restart
                && matches!(
                    (&ack.evidence, record.stage.as_str()),
                    (
                        ComputeControlDrainEvidence::ConditionalStartupFenced { .. },
                        "starting"
                    ) | (
                        ComputeControlDrainEvidence::ConditionalStopFenced { .. },
                        "stopping"
                    )
                )
                && record.lease.is_some()
                && *checkpoint == record.checkpoint
                && record.has_conditional_compute_write()
                && match record.scope {
                    UserAppOperationScope::Prod => {
                        serde_json::from_value::<UserAppComputeStartTarget>(checkpoint.clone())
                            .is_ok_and(|target| {
                                !target.volumes.is_empty()
                                    && record
                                        .execution_context()
                                        .is_ok_and(|context| context == target.target.context)
                            })
                    }
                    UserAppOperationScope::Dev => {
                        serde_json::from_value::<BuilderControlTarget>(checkpoint.clone())
                            .is_ok_and(|target| {
                                target.validate().is_ok()
                                    && target.workload.as_ref().is_some_and(|workload| {
                                        workload.kind == AppResourceKind::StatefulSet
                                    })
                                    && record
                                        .execution_context()
                                        .is_ok_and(|context| context == target.context)
                            })
                    }
                    _ => false,
                } => {}
        ComputeControlDrainEvidence::PersistedMutationBoundary { checkpoint }
            if record.action == ComputeControlAction::Restart
                && matches!(record.stage.as_str(), "stopped" | "verifying")
                && record.lease.is_some()
                && checkpoint.is_object()
                && *checkpoint == record.checkpoint => {}
        _ => {
            return Err(invalid(
                "Superseded compute executor has no confirmed drain evidence",
            ));
        }
    }
    // Preserve both the last stage and prior checkpoint for audit/recovery. Only
    // the original execution identity can be closed. An observer may use a
    // durable post-write boundary or an explicit conditional-write fence;
    // elapsed time and an in-flight stage alone are never sufficient.
    let checkpoint = serde_json::json!({"previous":record.checkpoint,"drain":ack.evidence});
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='failed',stage='superseded_drained',checkpoint_json=$2,error_code='ERR_CANCELLED',error_message='Superseded executor confirmed drained',revision=revision+1,updated_at_us=$3,terminal_at_us=$3 WHERE operation_id=$1 AND revision=$4 AND state='superseded' AND executor_id=$5"))
        .bind(&identity.operation_id).bind(serde_json::to_string(&checkpoint).map_err(storage)?)
        .bind(chrono::Utc::now().timestamp_micros()).bind(record.revision).bind(&identity.executor_id).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    compute::get(tx, &identity.app_id, &identity.operation_id)
        .await?
        .ok_or(Error::NotFound)
}

pub(super) async fn scan(
    tx: &mut dyn Executor,
    backend: Backend,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<ComputeControlRecord>, Error> {
    if limit == 0 || limit > 1000 {
        return Err(invalid("Invalid compute scan page size"));
    }
    let archive_pending = match backend {
        Backend::Postgres => {
            "(checkpoint_json::jsonb -> 'builder_restart_template') IS NOT NULL AND COALESCE(checkpoint_json::jsonb ->> 'builder_restart_archive_cleaned', 'false') <> 'true'"
        }
        Backend::Turso => {
            "json_extract(checkpoint_json, '$.builder_restart_template') IS NOT NULL AND COALESCE(json_extract(checkpoint_json, '$.builder_restart_archive_cleaned'), 0) <> 1"
        }
    };
    let query = format!(
        "SELECT operation_id FROM userapp_compute_controls WHERE operation_id>$1 AND (state IN ('pending','running','recovery_required') OR (state='superseded' AND executor_id IS NOT NULL) OR lease_json IS NOT NULL OR (state='succeeded' AND {archive_pending})) ORDER BY operation_id LIMIT $2"
    );
    let ids = repo::strings(
        toasty::sql::query(repo::sql(backend, &query))
            .bind(after.unwrap_or(""))
            .bind(i64::from(limit))
            .exec(tx)
            .await
            .map_err(storage)?,
    )?;
    let mut records = Vec::with_capacity(ids.len());
    for id in ids {
        let row = crate::db::models::ComputeControl::filter_by_operation_id(id)
            .first()
            .exec(tx)
            .await
            .map_err(storage)?
            .ok_or(Error::NotFound)?;
        records.push(compute::decode(row)?);
    }
    Ok(records)
}

/// Final evidence is produced after the original executor has awaited its last
/// mutation. Preserve that evidence and record cancellation, never old success.
/// The physical lease is deliberately retained for exact-receipt release.
pub(super) async fn finalize_interrupted(
    tx: &mut dyn Executor,
    backend: Backend,
    identity: &ComputeExecutorIdentity,
    snapshot: &UserAppOperationRecord,
) -> Result<UserAppOperationRecord, Error> {
    repo::claim_app(tx, backend, &identity.app_id).await?;
    let control = compute::current(tx, identity).await?;
    if !owns(&control, identity)
        || !(control.state == ComputeControlState::Running
            || (control.state == ComputeControlState::RecoveryRequired
                && control.can_recover_compute_drain()))
    {
        return Err(Error::VersionConflict);
    }
    let mut current = repo::operation(tx, &identity.app_id, &snapshot.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if current != *snapshot
        || current.lifecycle_id != identity.lifecycle_id
        || current.kind.ends_lifecycle()
        || !control
            .interrupted_operations
            .contains(&current.operation_id)
        || !matches!(
            current.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        )
        || !userapp_operation_has_drain_evidence(&current)
    {
        return Err(Error::VersionConflict);
    }
    // Reclaim only a proven final receipt, inside this transaction. No runtime
    // writes are authorized by this internal transition out of recovery.
    if current.state == UserAppOperationState::RecoveryRequired {
        let previous = current.clone();
        current.state = UserAppOperationState::Running;
        current.revision = current
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("Operation revision exhausted"))?;
        repo::save_operation(tx, backend, &current, &previous).await?;
    }
    let executor_id = current.executor_id.clone().ok_or(Error::VersionConflict)?;
    super::ops::advance(
        tx,
        backend,
        &UserAppOperationProgress {
            app_id: current.app_id,
            lifecycle_id: current.lifecycle_id,
            operation_id: current.operation_id,
            expected_revision: current.revision,
            executor_id,
            state: UserAppOperationState::Failed,
            step: "interrupted_effects_confirmed".into(),
            checkpoint: current.checkpoint,
            error_code: Some("COMPUTE_INTERRUPTED".into()),
            error_message: Some(format!(
                "Interrupted by compute operation {} after effects were confirmed",
                identity.operation_id
            )),
        },
    )
    .await
}

/// Stopped/Verifying are written only after the physical operation returned.
/// A stale observer cannot close a superseding control or an updated record.
pub(super) async fn finalize_confirmed(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
) -> Result<ComputeControlRecord, Error> {
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
    let identity = ComputeExecutorIdentity {
        app_id: snapshot.app_id.clone(),
        lifecycle_id: snapshot.lifecycle_id.clone(),
        operation_id: snapshot.operation_id.clone(),
        scope: snapshot.scope,
        generation: snapshot.generation,
        executor_id: snapshot.executor_id.clone().ok_or(Error::VersionConflict)?,
    };
    let current = compute::current(tx, &identity).await?;
    if current != *snapshot
        || current.lease.is_none()
        || !matches!(
            current.state,
            ComputeControlState::Running | ComputeControlState::RecoveryRequired
        )
        || !matches!(
            (current.action, current.stage.as_str()),
            (ComputeControlAction::Stop, "stopped") | (ComputeControlAction::Restart, "verifying")
        )
    {
        return Err(Error::VersionConflict);
    }
    // Preserve the original executor, generation, checkpoint and lease. This
    // internal claim is solely for terminal commit and is not visible mid-tx.
    let revision = current
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("Compute revision exhausted"))?;
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='running',revision=$3 WHERE operation_id=$1 AND revision=$2"))
        .bind(&current.operation_id).bind(current.revision).bind(revision)
        .exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    advance(
        tx,
        backend,
        &ComputeControlProgress {
            identity,
            expected_revision: revision,
            state: ComputeControlState::Succeeded,
            stage: ComputeControlStage::Completed,
            checkpoint: current.checkpoint,
            error_code: None,
            error_message: None,
        },
    )
    .await
}

/// The coordinator verified the atomic remote stop receipt. No mutation is replayed.
pub(super) async fn finalize_observed_stop(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
) -> Result<ComputeControlRecord, Error> {
    finalize_observed(tx, backend, snapshot, ComputeControlAction::Stop, None).await
}

pub(super) async fn finalize_observed_restart(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
) -> Result<ComputeControlRecord, Error> {
    finalize_observed(tx, backend, snapshot, ComputeControlAction::Restart, None).await
}

pub(super) async fn finalize_observed_creation(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    observed: &BuilderControlTarget,
    volumes: &[AppResourceIdentity],
) -> Result<ComputeControlRecord, Error> {
    let original: BuilderControlTarget =
        BuilderControlTarget::from_observation_checkpoint(&snapshot.checkpoint).map_err(storage)?;
    original.validate().map_err(Error::InvalidOperation)?;
    observed.validate().map_err(Error::InvalidOperation)?;
    let expected: Vec<AppResourceIdentity> = serde_json::from_value(
        snapshot
            .checkpoint
            .get("builder_volumes")
            .cloned()
            .ok_or_else(|| invalid("Original retained volumes missing"))?,
    )
    .map_err(storage)?;
    if !retained_builder_volume_evidence(snapshot)
        || original.workload.is_some()
        || original.pod.is_some()
        || original.resource_binding.is_some()
        || observed.context != original.context
        || observed
            .workload
            .as_ref()
            .is_none_or(|workload| workload.kind != AppResourceKind::StatefulSet)
        || observed.pod.is_none()
        || observed
            .resource_binding
            .as_ref()
            .is_some_and(|binding| binding.service_type != ServiceType::UserappBuilder)
        || observed.restart_image != original.restart_image
        || observed.restart_runtime_workspace != original.restart_runtime_workspace
        || observed.builder_startup_mode != original.builder_startup_mode
        || volumes != expected
    {
        return Err(invalid(
            "Observed builder creation differs from original intent or retained data",
        ));
    }
    finalize_observed(
        tx,
        backend,
        snapshot,
        ComputeControlAction::Restart,
        Some(observed),
    )
    .await
}

async fn finalize_observed(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    action: ComputeControlAction,
    creation: Option<&BuilderControlTarget>,
) -> Result<ComputeControlRecord, Error> {
    let (expected_stage, confirmed_stage) = match action {
        ComputeControlAction::Stop => ("stopping", "stopped"),
        ComputeControlAction::Restart => ("starting", "verifying"),
    };
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
    let context = snapshot
        .execution_context()
        .map_err(|error| invalid(&error))?;
    let valid_target = if action == ComputeControlAction::Stop {
        let observed =
            ComputeStopObservation::from_checkpoint(&snapshot.checkpoint, snapshot.scope)
                .map_err(storage)?;
        let same_context = observed.context() == &context;
        match observed {
            ComputeStopObservation::Absent(_) => same_context,
            ComputeStopObservation::Orphan(orphan) => {
                orphan.validate().map_err(Error::InvalidOperation)?;
                same_context
            }
            ComputeStopObservation::Builder(target) => {
                target.validate().map_err(Error::InvalidOperation)?;
                same_context
                    && target.workload.as_ref().is_some_and(|workload| {
                        matches!(
                            workload.kind,
                            AppResourceKind::StatefulSet | AppResourceKind::Container
                        )
                    })
            }
            ComputeStopObservation::Application(target) => {
                same_context
                    && matches!(
                        target.resource.kind,
                        AppResourceKind::Deployment | AppResourceKind::Container
                    )
            }
        }
    } else {
        match snapshot.scope {
            UserAppOperationScope::Prod => {
                let target: UserAppMutationTarget =
                    serde_json::from_value(snapshot.checkpoint.clone()).map_err(storage)?;
                target.context == context
                    && (target.resource.kind == AppResourceKind::Deployment
                        || target.resource.kind == AppResourceKind::Container)
            }
            UserAppOperationScope::Dev => {
                let target: BuilderControlTarget =
                    BuilderControlTarget::from_observation_checkpoint(&snapshot.checkpoint)
                        .map_err(storage)?;
                target.validate().map_err(Error::InvalidOperation)?;
                target.context == context
                    && (creation.is_some()
                        || target.workload.as_ref().is_some_and(|workload| {
                            workload.kind == AppResourceKind::StatefulSet
                                || workload.kind == AppResourceKind::Container
                        }))
            }
            _ => false,
        }
    };
    if snapshot.action != action
        || snapshot.stage != expected_stage
        || snapshot.lease.is_none()
        || !matches!(
            snapshot.state,
            ComputeControlState::Running | ComputeControlState::RecoveryRequired
        )
        || !valid_target
    {
        return Err(invalid("Invalid observed compute recovery snapshot"));
    }
    let identity = ComputeExecutorIdentity {
        app_id: snapshot.app_id.clone(),
        lifecycle_id: snapshot.lifecycle_id.clone(),
        operation_id: snapshot.operation_id.clone(),
        scope: snapshot.scope,
        generation: snapshot.generation,
        executor_id: context.executor_id,
    };
    if compute::current(tx, &identity).await? != *snapshot {
        return Err(Error::VersionConflict);
    }
    let revision = snapshot
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("Compute revision exhausted"))?;
    let mut checkpoint = snapshot.checkpoint.clone();
    if let Some(observed) = creation {
        let target = serde_json::to_value(observed).map_err(storage)?;
        for (key, value) in target
            .as_object()
            .ok_or_else(|| invalid("Observed builder target is not an object"))?
        {
            checkpoint[key] = value.clone();
        }
    }
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='running',stage=$4,revision=$3,checkpoint_json=$5 WHERE operation_id=$1 AND revision=$2"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(revision).bind(confirmed_stage)
        .bind(serde_json::to_string(&checkpoint).map_err(storage)?)
        .exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    let confirmed = compute::current(tx, &identity).await?;
    finalize_confirmed(tx, backend, &confirmed).await
}

/// Only a runtime-adapter observation under the original FD lease may bridge
/// Docker Restart/Stopping. This transaction does not restore, create, start,
/// release the lease, or manufacture the missing Stop acknowledgement.
pub(super) async fn confirm_builder_restart_stopped(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    observation: &BuilderRestartStopObservation,
) -> Result<ComputeControlRecord, Error> {
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
    observation.validate().map_err(Error::InvalidOperation)?;
    let context = snapshot
        .execution_context()
        .map_err(|error| invalid(&error))?;
    let identity = ComputeExecutorIdentity {
        app_id: snapshot.app_id.clone(),
        lifecycle_id: snapshot.lifecycle_id.clone(),
        scope: snapshot.scope,
        operation_id: snapshot.operation_id.clone(),
        generation: snapshot.generation,
        executor_id: context.executor_id.clone(),
    };
    let lease = snapshot.lease.as_ref().ok_or(Error::VersionConflict)?;
    validate_receipt(&identity, lease)?;
    let docker_lease = match lease {
        UserAppOperationLeaseReceipt::Docker { service_type, .. } => {
            service_type == &ServiceType::UserappBuilder
        }
        UserAppOperationLeaseReceipt::Kubernetes { .. } => false,
    };
    let original =
        BuilderControlTarget::from_observation_checkpoint(&snapshot.checkpoint).map_err(storage)?;
    let volumes: Vec<AppResourceIdentity> = serde_json::from_value(
        snapshot
            .checkpoint
            .get("builder_volumes")
            .cloned()
            .ok_or_else(|| invalid("Original builder mount witness missing"))?,
    )
    .map_err(storage)?;
    let eligible_state = match snapshot.state {
        ComputeControlState::Running | ComputeControlState::RecoveryRequired => true,
        ComputeControlState::Pending
        | ComputeControlState::Succeeded
        | ComputeControlState::Failed
        | ComputeControlState::Superseded => false,
    };
    if snapshot.scope != UserAppOperationScope::Dev
        || snapshot.action != ComputeControlAction::Restart
        || snapshot.stage != "stopping"
        || !eligible_state
        || !docker_lease
        || original != observation.target
        || original.context != context
        || volumes != observation.volumes
    {
        return Err(invalid("Invalid original Docker Restart stop observation"));
    }
    // Exact equality protects the original target, archive reference, mounts,
    // lease, diagnostics, executor and revision against every stale observer.
    if compute::current(tx, &identity).await? != *snapshot {
        return Err(Error::VersionConflict);
    }
    drained(tx, snapshot).await?;
    let revision = snapshot
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("Compute revision exhausted"))?;
    let mut checkpoint = snapshot.checkpoint.clone();
    let object = checkpoint
        .as_object_mut()
        .ok_or_else(|| invalid("Builder checkpoint must be an object"))?;
    let previous_observation = object.get(BUILDER_RESTART_STOP_OBSERVATION_KEY).cloned();
    object.insert(
        BUILDER_RESTART_STOP_OBSERVATION_KEY.into(),
        serde_json::to_value(BuilderRestartStopRecoveryEvidence {
            observation: observation.clone(),
            original_state: snapshot.state,
            original_error_code: snapshot.error_code.clone(),
            original_error_message: snapshot.error_message.clone(),
            original_restart_template: snapshot.checkpoint.get("builder_restart_template").cloned(),
            unrebuildable: None,
            previous_observation,
        })
        .map_err(storage)?,
    );
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET stage='stopped',checkpoint_json=$3,revision=$6,updated_at_us=$4 WHERE operation_id=$1 AND revision=$2 AND executor_id=$5 AND stage='stopping' AND state IN ('running','recovery_required')"))
        .bind(&snapshot.operation_id).bind(snapshot.revision)
        .bind(serde_json::to_string(&checkpoint).map_err(storage)?)
        .bind(chrono::Utc::now().timestamp_micros()).bind(&context.executor_id).bind(revision)
        .exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    compute::current(tx, &identity).await
}

/// Exact old-CID absence closes an unrebuildable pre-restore Stopping execution.
/// An already-Stopped row can hide an unknown Create and cannot be settled here.
/// This submits no current Source work and preserves the opaque old recipe.
pub(super) async fn fail_unrebuildable_builder_restart(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    observation: &BuilderRestartStopObservation,
    unavailable: Option<&BuilderRestartArchiveUnavailable>,
) -> Result<ComputeControlRecord, Error> {
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
    observation.validate().map_err(Error::InvalidOperation)?;
    let context = snapshot
        .execution_context()
        .map_err(|error| invalid(&error))?;
    let identity = ComputeExecutorIdentity {
        app_id: snapshot.app_id.clone(),
        lifecycle_id: snapshot.lifecycle_id.clone(),
        scope: snapshot.scope,
        operation_id: snapshot.operation_id.clone(),
        generation: snapshot.generation,
        executor_id: context.executor_id.clone(),
    };
    let lease = snapshot.lease.as_ref().ok_or(Error::VersionConflict)?;
    validate_receipt(&identity, lease)?;
    let docker_lease = match lease {
        UserAppOperationLeaseReceipt::Docker { service_type, .. } => {
            service_type == &ServiceType::UserappBuilder
        }
        UserAppOperationLeaseReceipt::Kubernetes { .. } => false,
    };
    let original =
        BuilderControlTarget::from_observation_checkpoint(&snapshot.checkpoint).map_err(storage)?;
    let volumes: Vec<AppResourceIdentity> = serde_json::from_value(
        snapshot
            .checkpoint
            .get("builder_volumes")
            .cloned()
            .ok_or_else(|| invalid("Original builder mounts missing"))?,
    )
    .map_err(storage)?;
    let eligible_state = match snapshot.state {
        ComputeControlState::Running | ComputeControlState::RecoveryRequired => true,
        ComputeControlState::Pending
        | ComputeControlState::Succeeded
        | ComputeControlState::Failed
        | ComputeControlState::Superseded => false,
    };
    if snapshot.scope != UserAppOperationScope::Dev
        || snapshot.action != ComputeControlAction::Restart
        || snapshot.stage != "stopping"
        || !eligible_state
        || !docker_lease
        || observation.kind != BuilderRestartStopKind::CapturedContainerAbsent
        || original != observation.target
        || original.context != context
        || volumes != observation.volumes
    {
        return Err(invalid(
            "Unrebuildable restart requires original pre-start Docker Container absence",
        ));
    }
    let reference = snapshot.checkpoint.get("builder_restart_template").cloned();
    let reason = match &reference {
        None if unavailable.is_none() => BuilderRestartUnrebuildableReason::MissingArchive,
        None => {
            return Err(invalid(
                "Unavailable recipe witness has no original reference",
            ));
        }
        Some(value) => match serde_json::from_value::<BuilderRestartTemplate>(value.clone()) {
            Err(_) if unavailable.is_none() => BuilderRestartUnrebuildableReason::UnreadableArchive,
            Err(_) => {
                return Err(invalid(
                    "Unavailable recipe witness cannot replace an unreadable reference",
                ));
            }
            Ok(template)
                if template.source != original
                    || template.volumes != volumes
                    || template.archive.kind != AppResourceKind::File
                    || template.archive.name.is_empty()
                    || template.archive.uid.is_empty() =>
            {
                if unavailable.is_some() {
                    return Err(invalid(
                        "An unbound historical recipe cannot borrow a runtime witness",
                    ));
                }
                // It cannot be decoded as this original operation's recipe;
                // preserve it, and never inspect another application's archive.
                BuilderRestartUnrebuildableReason::UnreadableArchive
            }
            Ok(template) => {
                let witness = unavailable.ok_or_else(|| {
                    invalid("A readable recipe needs positive runtime unavailability")
                })?;
                match witness.kind {
                    BuilderRestartArchiveUnavailableKind::PositiveUnavailable => {}
                }
                if witness.template != template
                    || serde_json::to_value(&witness.template).map_err(storage)? != *value
                    || witness.template.source != original
                    || witness.template.volumes != volumes
                    || witness.template.archive.kind != AppResourceKind::File
                    || witness.template.archive.name.is_empty()
                    || witness.template.archive.uid.is_empty()
                {
                    return Err(invalid(
                        "Unavailable recipe witness differs from the original private archive",
                    ));
                }
                BuilderRestartUnrebuildableReason::ArchiveUnavailable
            }
        },
    };
    if compute::current(tx, &identity).await? != *snapshot {
        return Err(Error::VersionConflict);
    }
    drained(tx, snapshot).await?;
    let mut checkpoint = snapshot.checkpoint.clone();
    let mut evidence = BuilderRestartStopRecoveryEvidence {
        observation: observation.clone(),
        original_state: snapshot.state,
        original_error_code: snapshot.error_code.clone(),
        original_error_message: snapshot.error_message.clone(),
        original_restart_template: reference,
        unrebuildable: None,
        previous_observation: checkpoint
            .get(BUILDER_RESTART_STOP_OBSERVATION_KEY)
            .cloned(),
    };
    evidence.unrebuildable = Some(BuilderRestartUnrebuildableEvidence {
        reason,
        observation: observation.clone(),
        archive_unavailable: unavailable.cloned(),
    });
    checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY] =
        serde_json::to_value(evidence).map_err(storage)?;
    let revision = snapshot
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("Compute revision exhausted"))?;
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='failed',stage='stopped',checkpoint_json=$3,revision=$4,updated_at_us=$5,terminal_at_us=$5 WHERE operation_id=$1 AND revision=$2 AND executor_id=$6 AND stage='stopping' AND state IN ('running','recovery_required')"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(serde_json::to_string(&checkpoint).map_err(storage)?)
        .bind(revision).bind(chrono::Utc::now().timestamp_micros()).bind(&context.executor_id)
        .exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    compute::get(tx, &snapshot.app_id, &snapshot.operation_id)
        .await?
        .ok_or(Error::NotFound)
}

/// Atomically assigns the remaining start step, preserving operation and lease.
pub(super) async fn resume_stopped_restart(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    target: &UserAppMutationTarget,
) -> Result<ComputeControlRecord, Error> {
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
    let context = snapshot.execution_context().map_err(|e| invalid(&e))?;
    let old: UserAppMutationTarget =
        serde_json::from_value(snapshot.checkpoint.clone()).map_err(storage)?;
    let restored = if snapshot.stage == "stopped" && old.resource.uid != target.resource.uid {
        match old.resource.kind {
            AppResourceKind::Deployment => snapshot
                .checkpoint
                .get("app_restart_template")
                .cloned()
                .and_then(|value| serde_json::from_value::<AppRestartTemplate>(value).ok())
                .is_some_and(|archive| {
                    archive.source == old
                        && archive.archive.kind == AppResourceKind::Secret
                        && !archive.archive.uid.is_empty()
                }),
            AppResourceKind::Container => {
                serde_json::from_value::<UserAppComputeStartTarget>(snapshot.checkpoint.clone())
                    .ok()
                    .is_some_and(|captured| {
                        !captured.volumes.is_empty()
                            && captured
                                .restart_image
                                .as_deref()
                                .is_some_and(|image| !image.is_empty())
                    })
            }
            _ => false,
        }
    } else {
        false
    };
    if snapshot.action != ComputeControlAction::Restart
        || snapshot.scope != UserAppOperationScope::Prod
        || !matches!(snapshot.stage.as_str(), "stopped" | "starting")
        || snapshot.lease.is_none()
        || !matches!(
            snapshot.state,
            ComputeControlState::Running | ComputeControlState::RecoveryRequired
        )
        || old.context != context
        || target.context != context
        || !(target.resource.kind == AppResourceKind::Deployment
            || (snapshot.stage == "stopped" && target.resource.kind == AppResourceKind::Container))
        || old.resource.kind != target.resource.kind
        || target.resource.uid.is_empty()
        || (old.resource.uid != target.resource.uid && !restored)
        || old.resource.name != target.resource.name
    {
        return Err(invalid("Invalid restart continuation target"));
    }
    if snapshot.stage == "starting" {
        let captured: UserAppComputeStartTarget =
            serde_json::from_value(snapshot.checkpoint.clone()).map_err(storage)?;
        if !captured.compute_start_single_write || captured.volumes.is_empty() {
            return Err(invalid(
                "Restart retry requires the original single-write volume witness",
            ));
        }
    }
    let identity = ComputeExecutorIdentity {
        app_id: snapshot.app_id.clone(),
        lifecycle_id: snapshot.lifecycle_id.clone(),
        operation_id: snapshot.operation_id.clone(),
        scope: snapshot.scope,
        generation: snapshot.generation,
        executor_id: context.executor_id,
    };
    if compute::current(tx, &identity).await? != *snapshot {
        return Err(Error::VersionConflict);
    }
    let revision = snapshot
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("Compute revision exhausted"))?;
    let mut checkpoint = snapshot.checkpoint.clone();
    checkpoint["resource"] = serde_json::to_value(&target.resource).map_err(storage)?;
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='running',stage='starting',checkpoint_json=$4,error_code=NULL,error_message=NULL,revision=$3 WHERE operation_id=$1 AND revision=$2"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(revision)
        .bind(serde_json::to_string(&checkpoint).map_err(storage)?)
        .exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    compute::current(tx, &identity).await
}

/// Archive bookkeeping cannot grant a new runtime write. The caller verifies
/// the original physical stop/lease/volumes; this transaction retains its exact
/// execution and current-head CAS before any restore may be dispatched.
pub(super) async fn capture_stopped_builder_template(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    source: &BuilderControlTarget,
    template: &BuilderRestartTemplate,
) -> Result<ComputeControlRecord, Error> {
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
    let context = snapshot
        .execution_context()
        .map_err(|error| invalid(&error))?;
    let original: BuilderControlTarget =
        serde_json::from_value(snapshot.checkpoint.clone()).map_err(storage)?;
    original.validate().map_err(Error::InvalidOperation)?;
    source.validate().map_err(Error::InvalidOperation)?;
    let old = original
        .workload
        .as_ref()
        .ok_or_else(|| invalid("Original retained builder missing"))?;
    let current = source
        .workload
        .as_ref()
        .ok_or_else(|| invalid("Retained builder source missing"))?;
    let volumes: Vec<AppResourceIdentity> = serde_json::from_value(
        snapshot
            .checkpoint
            .get("builder_volumes")
            .cloned()
            .ok_or_else(|| invalid("Original builder volume witness missing"))?,
    )
    .map_err(storage)?;
    if snapshot.scope != UserAppOperationScope::Dev
        || snapshot.action != ComputeControlAction::Restart
        || snapshot.stage != "stopped"
        || !matches!(
            snapshot.state,
            ComputeControlState::Running | ComputeControlState::RecoveryRequired
        )
        || snapshot.lease.is_none()
        || old.kind != AppResourceKind::Container
        || old.kind != current.kind
        || old.uid != current.uid
        || old.name != current.name
        || original.context != context
        || source.context != context
        || original.resource_binding != source.resource_binding
        || original.restart_image != source.restart_image
        || original.restart_runtime_workspace != source.restart_runtime_workspace
        || original.builder_startup_mode != source.builder_startup_mode
        || source.pod.is_some()
        || template.source != *source
        || template.archive.kind != AppResourceKind::File
        || template.archive.name.is_empty()
        || template.archive.uid.is_empty()
        || volumes.is_empty()
        || template.volumes != volumes
    {
        return Err(invalid("Invalid stopped builder archive capture witness"));
    }
    let identity = ComputeExecutorIdentity {
        app_id: snapshot.app_id.clone(),
        lifecycle_id: snapshot.lifecycle_id.clone(),
        operation_id: snapshot.operation_id.clone(),
        scope: snapshot.scope,
        generation: snapshot.generation,
        executor_id: context.executor_id,
    };
    if compute::current(tx, &identity).await? != *snapshot {
        return Err(Error::VersionConflict);
    }
    drained(tx, snapshot).await?;
    let revision = snapshot
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("Compute revision exhausted"))?;
    let mut checkpoint = snapshot.checkpoint.clone();
    let prior = checkpoint.get("builder_restart_template").cloned();
    let object = checkpoint
        .as_object_mut()
        .ok_or_else(|| invalid("Builder checkpoint must be an object"))?;
    // Historical reference values stay in the same private operation ledger.
    // No old archive bytes are rewritten or deleted by this transaction.
    let history = object
        .entry("builder_restart_template_history")
        .or_insert_with(|| serde_json::json!([]));
    history
        .as_array_mut()
        .ok_or_else(|| invalid("Builder restart reference history must be an array"))?
        .push(serde_json::json!({"was_present": prior.is_some(), "value": prior}));
    object.insert(
        "builder_restart_template".into(),
        serde_json::to_value(template).map_err(storage)?,
    );
    for (key, value) in serde_json::to_value(source)
        .map_err(storage)?
        .as_object()
        .ok_or_else(|| invalid("Builder source must be an object"))?
    {
        object.insert(key.clone(), value.clone());
    }
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET checkpoint_json=$4,revision=$3,updated_at_us=$5 WHERE operation_id=$1 AND revision=$2"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(revision)
        .bind(serde_json::to_string(&checkpoint).map_err(storage)?)
        .bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    compute::current(tx, &identity).await
}

pub(super) async fn resume_builder_restart(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    target: &BuilderControlTarget,
) -> Result<ComputeControlRecord, Error> {
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
    let context = snapshot.execution_context().map_err(|e| invalid(&e))?;
    let old: BuilderControlTarget =
        serde_json::from_value(snapshot.checkpoint.clone()).map_err(storage)?;
    old.validate().map_err(Error::InvalidOperation)?;
    target.validate().map_err(Error::InvalidOperation)?;
    let same_physical_intent = match (&old.workload, &target.workload) {
        (Some(original), Some(workload)) => {
            let restored = if snapshot.stage == "stopped" && original.uid != workload.uid {
                let template: BuilderRestartTemplate = serde_json::from_value(
                    snapshot
                        .checkpoint
                        .get("builder_restart_template")
                        .ok_or_else(|| invalid("Restart template missing"))?
                        .clone(),
                )
                .map_err(storage)?;
                template.source == old
                    && matches!(
                        (original.kind, template.archive.kind),
                        (AppResourceKind::StatefulSet, AppResourceKind::Secret)
                            | (AppResourceKind::Container, AppResourceKind::File)
                    )
                    && !template.archive.uid.is_empty()
                    && !template.volumes.is_empty()
                    && target.resource_binding.is_none()
            } else {
                false
            };
            (workload.kind == AppResourceKind::StatefulSet
                || (snapshot.stage == "stopped" && workload.kind == AppResourceKind::Container))
                && original.kind == workload.kind
                && (original.uid == workload.uid || restored)
                && original.name == workload.name
                && (old.resource_binding == target.resource_binding || restored)
        }
        (None, None) => {
            // An absent Kubernetes controller can be recreated against its
            // retained PVC without inventing an old workload UID or archive.
            // Runtime I/O verifies absence, ownership and these exact PVC UIDs
            // before this CAS and before the original receipt authorizes create.
            retained_builder_creation(snapshot) && target.resource_binding.is_none()
        }
        (Some(_), None) => stopped_builder_absence(snapshot, target),
        _ => false,
    };
    if snapshot.action != ComputeControlAction::Restart
        || snapshot.scope != UserAppOperationScope::Dev
        || !(snapshot.stage == "stopped"
            || (snapshot.stage == "starting"
                && (snapshot.has_conditional_compute_write()
                    || (old.workload.is_none() && target.workload.is_none()))))
        || snapshot.lease.is_none()
        || !matches!(
            snapshot.state,
            ComputeControlState::Running | ComputeControlState::RecoveryRequired
        )
        || old.context != context
        || target.context != context
        || !same_physical_intent
        || target.restart_image != old.restart_image
        || (target.restart_runtime_workspace != old.restart_runtime_workspace
            && !stopped_builder_absence(snapshot, target))
        || target.builder_startup_mode != old.builder_startup_mode
        || target.pod.is_some()
    {
        return Err(invalid("Invalid stopped builder continuation witness"));
    }
    let identity = ComputeExecutorIdentity {
        app_id: snapshot.app_id.clone(),
        lifecycle_id: snapshot.lifecycle_id.clone(),
        scope: snapshot.scope,
        operation_id: snapshot.operation_id.clone(),
        generation: snapshot.generation,
        executor_id: context.executor_id,
    };
    if compute::current(tx, &identity).await? != *snapshot {
        return Err(Error::VersionConflict);
    }
    let revision = snapshot
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("Compute revision exhausted"))?;
    let mut checkpoint = serde_json::to_value(target).map_err(storage)?;
    if target.workload.is_some() && snapshot.has_conditional_compute_write() {
        checkpoint["builder_compute_single_write"] = serde_json::Value::Bool(true);
    }
    if let Some(volumes) = snapshot.checkpoint.get("builder_volumes") {
        checkpoint["builder_volumes"] = volumes.clone();
    }
    if let Some(template) = snapshot.checkpoint.get("builder_restart_template") {
        checkpoint["builder_restart_template"] = template.clone();
    }
    if let Some(history) = snapshot.checkpoint.get("builder_restart_template_history") {
        checkpoint["builder_restart_template_history"] = history.clone();
    }
    if let Some(mode) = snapshot.checkpoint.get("builder_startup_mode") {
        checkpoint["builder_startup_mode"] = mode.clone();
    }
    // Historical observation JSON is diagnostic during an existing Stopped
    // continuation, not a prerequisite to its original physical/lease CAS.
    if let Some(value) = snapshot
        .checkpoint
        .get(BUILDER_RESTART_STOP_OBSERVATION_KEY)
    {
        checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY] = value.clone();
    }
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='running',stage='starting',checkpoint_json=$4,error_code=NULL,error_message=NULL,revision=$3 WHERE operation_id=$1 AND revision=$2"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(revision)
        .bind(serde_json::to_string(&checkpoint).map_err(storage)?).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    compute::current(tx, &identity).await
}

/// The caller already deleted the exact archive; only private cleanup metadata changes.
pub(super) async fn mark_archive_cleaned(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
) -> Result<(), Error> {
    let record = compute::get(tx, &snapshot.app_id, &snapshot.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if record != *snapshot
        || record.state != ComputeControlState::Succeeded
        || record.lease.is_some()
        || record.checkpoint.get("builder_restart_template").is_none()
    {
        return Err(Error::VersionConflict);
    }
    let mut checkpoint = record.checkpoint.clone();
    checkpoint["builder_restart_archive_cleaned"] = serde_json::Value::Bool(true);
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET checkpoint_json=$3,revision=revision+1,updated_at_us=$4 WHERE operation_id=$1 AND revision=$2 AND state='succeeded' AND lease_json IS NULL"))
        .bind(&record.operation_id).bind(record.revision).bind(serde_json::to_string(&checkpoint).map_err(storage)?)
        .bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    Ok(())
}
