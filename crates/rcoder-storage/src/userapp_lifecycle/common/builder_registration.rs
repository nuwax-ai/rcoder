//! Registration is part of a verified completed replacement, not a new
//! adoption request. The caller's short transaction also retires/rebinds the
//! project/container rows so neither half can commit alone.
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
