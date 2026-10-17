//! Recover a private archive only from the original retained stopped resource.
use anyhow::{Context as _, Result, ensure};
use container_runtime_api::{AgentContainerRuntime, UserAppDeploymentRuntime};
use shared_types::{
    BuilderControlTarget, BuilderRestartTemplate, ComputeControlRecord, ComputeControlState,
    UserAppLifecycleStore, UserAppOperationScope,
};

pub(super) async fn prepare<R, S>(
    runtime: &R,
    store: &S,
    snapshot: &ComputeControlRecord,
    original: &BuilderControlTarget,
    retained: &BuilderControlTarget,
) -> Result<(ComputeControlRecord, BuilderRestartTemplate)>
where
    R: AgentContainerRuntime + UserAppDeploymentRuntime + ?Sized,
    S: UserAppLifecycleStore + ?Sized,
{
    prepare_with_guard(runtime, store, snapshot, original, retained, None).await
}

pub(super) async fn prepare_with_guard<R, S>(
    runtime: &R,
    store: &S,
    snapshot: &ComputeControlRecord,
    original: &BuilderControlTarget,
    retained: &BuilderControlTarget,
    held_guard: Option<&dyn shared_types::AppOperationLease>,
) -> Result<(ComputeControlRecord, BuilderRestartTemplate)>
where
    R: AgentContainerRuntime + UserAppDeploymentRuntime + ?Sized,
    S: UserAppLifecycleStore + ?Sized,
{
    let expected: Vec<shared_types::AppResourceIdentity> = serde_json::from_value(
        snapshot
            .checkpoint
            .get("builder_volumes")
            .cloned()
            .context("Original builder volume witness missing")?,
    )?;
    ensure!(
        !expected.is_empty(),
        "Original builder volume witness is empty"
    );
    // Retries reuse the already captured private content, including after a
    // crash between its checkpoint CAS and the physical restore.
    if let Some(value) = snapshot.checkpoint.get("builder_restart_template")
        && let Ok(template) = serde_json::from_value::<BuilderRestartTemplate>(value.clone())
        && template.source == *original
        && template.volumes == expected
        && !template.archive.uid.is_empty()
        && !template.archive.name.is_empty()
        && original
            .workload
            .as_ref()
            .and_then(|resource| super::policy::restart_archive_kind(resource.kind))
            == Some(template.archive.kind)
        && runtime.inspect_builder_restart_archive(&template).await? != Some(false)
    {
        return Ok((snapshot.clone(), template));
    }
    // A positive corrupt-content observation is not an unknown write. It
    // still grants no recapture without every physical check below.
    ensure!(
        match snapshot.scope {
            UserAppOperationScope::Dev => true,
            UserAppOperationScope::Prod | UserAppOperationScope::Application => false,
        } && super::policy::is_restart(snapshot.action)
            && snapshot.stage == shared_types::ComputeControlStage::Stopped.as_str()
            && match snapshot.state {
                ComputeControlState::Running | ComputeControlState::RecoveryRequired => true,
                ComputeControlState::Pending
                | ComputeControlState::Succeeded
                | ComputeControlState::Failed
                | ComputeControlState::Superseded => false,
            },
        "Historical builder archive can only be recaptured at its original stopped boundary"
    );
    let context = snapshot.execution_context().map_err(anyhow::Error::msg)?;
    let receipt = snapshot
        .lease
        .as_ref()
        .context("Original builder lease missing")?;
    verify_retained(original, retained, &context)?;
    ensure!(
        original_lease_current(runtime, &context, receipt, held_guard).await?,
        "Original builder lease changed before archive recapture"
    );
    ensure!(
        runtime.reconcile_builder_compute_stop(original).await?,
        "Original builder stop is not confirmed before archive recapture"
    );
    ensure!(
        runtime.capture_builder_compute_volumes(retained).await? == expected,
        "Retained builder volumes changed before archive recapture"
    );
    let template = runtime.recapture_stopped_builder_restart(retained).await?;
    ensure!(
        template.source == *retained && template.volumes == expected,
        "Recaptured builder archive changed execution or volume identity"
    );
    let mut after = runtime
        .capture_bound_builder_control(&context, retained.resource_binding.as_ref())
        .await?;
    after.restart_image = retained.restart_image.clone();
    after.restart_runtime_workspace = retained.restart_runtime_workspace.clone();
    after.builder_startup_mode = retained.builder_startup_mode;
    ensure!(
        after == *retained,
        "Retained builder changed during archive recapture"
    );
    ensure!(
        original_lease_current(runtime, &context, receipt, held_guard).await?
            && runtime.reconcile_builder_compute_stop(original).await?
            && runtime.capture_builder_compute_volumes(retained).await? == expected,
        "Original builder stop, lease or volumes changed during archive recapture"
    );
    let persisted = store
        .capture_stopped_builder_restart_template(snapshot, retained, &template)
        .await?;
    // No restore/start occurs before this complete old-snapshot CAS commits.
    Ok((persisted, template))
}

async fn original_lease_current<R: UserAppDeploymentRuntime + ?Sized>(
    runtime: &R,
    context: &shared_types::UserAppExecutionContext,
    receipt: &shared_types::UserAppOperationLeaseReceipt,
    held_guard: Option<&dyn shared_types::AppOperationLease>,
) -> Result<bool> {
    if let Some(guard) = held_guard {
        ensure!(
            guard.receipt().as_ref() == Some(receipt),
            "Retained archive guard receipt differs"
        );
        return guard.validate_retained().await.map_err(anyhow::Error::msg);
    }
    runtime
        .validate_app_operation_receipt(context, receipt)
        .await
        .map_err(Into::into)
}

fn verify_retained(
    original: &BuilderControlTarget,
    retained: &BuilderControlTarget,
    context: &shared_types::UserAppExecutionContext,
) -> Result<()> {
    original.validate().map_err(anyhow::Error::msg)?;
    retained.validate().map_err(anyhow::Error::msg)?;
    let old = original
        .workload
        .as_ref()
        .context("Original builder physical identity missing")?;
    let current = retained.workload.as_ref().context(
        "Original retained builder disappeared; no current recipe may replace its archive",
    )?;
    ensure!(
        original.context == *context
            && retained.context == *context
            && old.uid == current.uid
            && old.name == current.name
            && old.kind == current.kind
            && original.resource_binding == retained.resource_binding
            && original.restart_image == retained.restart_image
            && original.restart_runtime_workspace == retained.restart_runtime_workspace
            && original.builder_startup_mode == retained.builder_startup_mode
            && retained.pod.is_none(),
        "Original retained builder or frozen restart intent changed before archive recapture"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
