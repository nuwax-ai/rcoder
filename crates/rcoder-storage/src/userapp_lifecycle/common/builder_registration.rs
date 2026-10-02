//! Completed replacements and explicit registration repairs share the root CAS.
//! The caller's short transaction also rebinds project/container rows so the
//! lifecycle outcome and registry cannot commit separately.
use super::{compute, ops, repo};
use crate::{db::schema::Backend, userapp_lifecycle::domain};
use anyhow::{Context, Result, ensure};
use shared_types::*;
use toasty::Executor;

pub(crate) async fn completed_builder_registration_candidates(
    tx: &mut dyn Executor,
    app_id: &str,
    lifecycle_id: &str,
    pod_uid: &str,
    workload_uid: &str,
) -> Result<Vec<UserAppOperationRecord>> {
    // Resource identity selects possible originals, never the newest operation
    // or its age. The engine must additionally verify each runtime creator receipt.
    let ids = repo::strings(toasty::sql::query("SELECT operation_id FROM userapp_operations WHERE app_id=$1 AND lifecycle_id=$2 AND kind='ensure_builder' AND state='succeeded' AND checkpoint_json::jsonb->>'container_id'=$3 AND checkpoint_json::jsonb->>'workload_uid'=$4")
        .bind(app_id).bind(lifecycle_id).bind(pod_uid).bind(workload_uid).exec(tx).await?)?;
    let mut result = Vec::new();
    for id in ids {
        result.push(
            repo::operation(tx, app_id, &id)
                .await?
                .context("Completed builder creator candidate disappeared")?,
        );
    }
    Ok(result)
}

pub(crate) async fn bind_completed_builder_registration(
    tx: &mut dyn Executor,
    operation: &UserAppOperationRecord,
    evidence: &BuilderCreationEvidence,
    volumes: &[AppResourceIdentity],
) -> Result<()> {
    evidence
        .validate_registration_replacement(operation, volumes)
        .map_err(anyhow::Error::msg)?;
    // Reuse the same root CAS as deletion and priority compute admission. No
    // runtime I/O occurs while this transaction owns the token.
    repo::claim_app(tx, Backend::Postgres, &operation.app_id).await?;
    compute::check_access(
        tx,
        Backend::Postgres,
        &operation.app_id,
        UserAppOperationScope::Dev,
        false,
    )
    .await?;
    let app = repo::app(tx, &operation.app_id)
        .await?
        .context("Builder lifecycle disappeared")?;
    domain::validate_active(&app)?;
    ensure!(
        app.lifecycle_id == operation.lifecycle_id,
        "Builder registration lifecycle changed"
    );
    let current = repo::operation(tx, &operation.app_id, &operation.operation_id)
        .await?
        .context("Builder completion operation disappeared")?;
    ensure!(
        current == *operation,
        "Builder completion operation changed before registration"
    );
    if let Some(predecessor) = &evidence.registration_predecessor {
        let stored = current
            .checkpoint
            .get("builder_creation_evidence")
            .context("Completed builder registration evidence missing")?;
        ensure!(
            *stored == serde_json::to_value(evidence)?,
            "Builder registration evidence differs from the durable operation"
        );
        let source = predecessor
            .target
            .workload
            .as_ref()
            .context("Builder predecessor workload missing")?;
        if let Some(old) = ops::get_resource_binding(
            tx,
            Backend::Postgres,
            &ServiceType::UserappBuilder,
            &source.uid,
        )
        .await?
        {
            old.validate(&evidence.target.context, &source.uid)
                .map_err(anyhow::Error::msg)?;
        }
    } else {
        ensure!(
            current
                .checkpoint
                .get("builder_creation_evidence")
                .is_none(),
            "Legacy registration recovery cannot discard a recorded predecessor proof"
        );
        // Older success checkpoints contain the returned resource only. The
        // original runtime receipt supplies the creator binding; no old PVC or
        // exit witness is manufactured during this registration-only repair.
        let stored: ContainerBasicInfo = serde_json::from_value(current.checkpoint.clone())
            .context("Decode original builder result for registration recovery")?;
        ensure!(
            stored.container_id == evidence.container.container_id
                && stored.workload_uid == evidence.container.workload_uid
                && stored.container_name == evidence.container.container_name,
            "Original builder result differs from its runtime creator receipt"
        );
    }
    let workload = evidence
        .target
        .workload
        .as_ref()
        .context("Builder replacement workload missing")?;
    let binding = UserAppResourceBinding {
        app_id: operation.app_id.clone(),
        lifecycle_id: operation.lifecycle_id.clone(),
        service_type: ServiceType::UserappBuilder,
        physical_uid: workload.uid.clone(),
        adopted_by_operation: operation.operation_id.clone(),
    };
    toasty::sql::statement("INSERT INTO userapp_resource_bindings(service_type,physical_uid,app_id,lifecycle_id,adopted_by_operation,created_at_us) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(service_type,physical_uid) DO NOTHING")
        .bind(binding.service_type.to_string()).bind(&binding.physical_uid)
        .bind(&binding.app_id).bind(&binding.lifecycle_id).bind(&binding.adopted_by_operation)
        .bind(chrono::Utc::now().timestamp_micros()).exec(tx).await?;
    let persisted = ops::get_resource_binding(
        tx,
        Backend::Postgres,
        &binding.service_type,
        &binding.physical_uid,
    )
    .await?
    .context("Builder replacement binding was not persisted")?;
    ensure!(
        persisted == binding,
        "Builder replacement binding belongs to another operation or lifecycle"
    );
    Ok(())
}

/// Explicit adoption may repair a pre-receipt registry without manufacturing
/// an old creation/exit receipt. This transaction grants only registration;
/// compute controls retain their own physical leases and domain checks.
pub(crate) async fn commit_builder_registration_adoption(
    tx: &mut dyn Executor,
    target: &BuilderControlTarget,
    expected_source_uid: &str,
    progress: &UserAppOperationProgress,
) -> Result<()> {
    target.validate().map_err(anyhow::Error::msg)?;
    let context = &target.context;
    let workload = target
        .workload
        .as_ref()
        .context("Adopted builder workload missing")?;
    ensure!(
        workload.kind == AppResourceKind::StatefulSet
            && !expected_source_uid.is_empty()
            && expected_source_uid != workload.uid
            && progress.app_id == context.app_id
            && progress.lifecycle_id == context.lifecycle_id
            && progress.operation_id == context.operation_id
            && progress.executor_id == context.executor_id
            && progress.state == UserAppOperationState::Succeeded,
        "Builder registry adoption identity differs"
    );
    repo::claim_app(tx, Backend::Postgres, &context.app_id).await?;
    compute::check_access(
        tx,
        Backend::Postgres,
        &context.app_id,
        UserAppOperationScope::Dev,
        false,
    )
    .await?;
    let app = repo::app(tx, &context.app_id)
        .await?
        .context("Builder lifecycle disappeared")?;
    domain::validate_active(&app)?;
    ensure!(
        app.lifecycle_id == context.lifecycle_id,
        "Builder adoption lifecycle changed"
    );
    let operation = repo::operation(tx, &context.app_id, &context.operation_id)
        .await?
        .context("Builder adoption operation disappeared")?;
    ensure!(
        operation.kind == UserAppOperationKind::AdoptBuilder
            && operation.state == UserAppOperationState::Running
            && operation.revision == progress.expected_revision
            && operation.executor_id.as_deref() == Some(context.executor_id.as_str())
            && operation.request_fingerprint == context.request_fingerprint,
        "Builder registry adoption operation changed"
    );
    let source = ops::get_resource_binding(
        tx,
        Backend::Postgres,
        &ServiceType::UserappBuilder,
        expected_source_uid,
    )
    .await?
    .context("Old builder registration has no lifecycle binding")?;
    source
        .validate(context, expected_source_uid)
        .map_err(anyhow::Error::msg)?;
    let binding = target
        .resource_binding
        .as_ref()
        .context("Adopted builder binding missing")?;
    binding
        .validate(context, &workload.uid)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        binding.service_type == ServiceType::UserappBuilder,
        "Adopted resource is not a builder"
    );
    let existing = ops::get_resource_binding(
        tx,
        Backend::Postgres,
        &ServiceType::UserappBuilder,
        &workload.uid,
    )
    .await?;
    if let Some(existing) = existing {
        ensure!(existing == *binding, "Adopted builder binding changed");
    } else {
        ensure!(
            binding.adopted_by_operation == context.operation_id,
            "New adoption binding has another operation"
        );
        toasty::sql::statement("INSERT INTO userapp_resource_bindings(service_type,physical_uid,app_id,lifecycle_id,adopted_by_operation,created_at_us) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(service_type,physical_uid) DO NOTHING")
            .bind(binding.service_type.to_string()).bind(&binding.physical_uid)
            .bind(&binding.app_id).bind(&binding.lifecycle_id).bind(&binding.adopted_by_operation)
            .bind(chrono::Utc::now().timestamp_micros()).exec(tx).await?;
        ensure!(
            ops::get_resource_binding(
                tx,
                Backend::Postgres,
                &ServiceType::UserappBuilder,
                &workload.uid
            )
            .await?
            .as_ref()
                == Some(binding),
            "Adopted builder binding belongs to another lifecycle"
        );
    }
    ops::advance(tx, Backend::Postgres, progress).await?;
    Ok(())
}
