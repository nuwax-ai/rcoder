//! Retain the operation's private restart snapshot at every startup entrypoint.
use anyhow::{Context as _, Result, ensure};
use container_runtime_api::{AgentContainerRuntime, UserAppDeploymentRuntime};
use shared_types::ContainerBasicInfo;
use shared_types::{BuilderControlTarget, BuilderRestartTemplate, BuilderStartOptions};

#[cfg(test)]
pub(super) async fn start_from_checkpoint<R: AgentContainerRuntime + ?Sized>(
    runtime: &R,
    target: &BuilderControlTarget,
    checkpoint: &serde_json::Value,
) -> Result<Option<ContainerBasicInfo>> {
    let options = prepare_start_options(runtime, target, checkpoint).await?;
    runtime
        .start_builder_control_with_options(target, &options)
        .await
        .context("Start builder from its original restart archive")
}

/// Resolve historical metadata before recording any possible runtime write.
pub(super) async fn prepare_start_options<R: AgentContainerRuntime + ?Sized>(
    runtime: &R,
    target: &BuilderControlTarget,
    checkpoint: &serde_json::Value,
) -> Result<BuilderStartOptions> {
    // Old archives describe a vanished controller. They are diagnostics only
    // once the current namespace is absent and its exact retained PVC is read.
    if target.workload.is_none() {
        confirm_missing_target(runtime, target, checkpoint).await?;
        return Ok(BuilderStartOptions {
            startup_mode: target.builder_startup_mode,
            restart_template: None,
        });
    }
    let template = match checkpoint.get("builder_restart_template") {
        None => None,
        Some(saved) => match serde_json::from_value::<BuilderRestartTemplate>(saved.clone()) {
            Ok(template) if template.source.context == target.context => Some(template),
            Ok(_) | Err(_) => {
                // A damaged old reference cannot keep a confirmed retained
                // controller stopped forever. The runtime still verifies its
                // physical target and volumes before starting that controller.
                ensure!(
                    target.workload.is_some()
                        && runtime.reconcile_builder_compute_stop(target).await?,
                    "Builder restart archive is unavailable and physical stop is unconfirmed"
                );
                tracing::warn!(operation_id = %target.context.operation_id,
                    "historical restart reference ignored after physical stop confirmation");
                None
            }
        },
    };
    Ok(BuilderStartOptions {
        startup_mode: target.builder_startup_mode,
        restart_template: template,
    })
}

/// A missing controller is ensured under the original compute lease. This
/// dispatch shares the normal startup path; no nested EnsureBuilder is admitted.
pub(super) async fn start_with_options<R: AgentContainerRuntime + ?Sized>(
    runtime: &R,
    target: &BuilderControlTarget,
    options: &BuilderStartOptions,
    checkpoint: &serde_json::Value,
    receipt: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<Option<ContainerBasicInfo>> {
    if target.workload.is_some() {
        return runtime
            .start_builder_control_with_options(target, options)
            .await
            .map_err(Into::into);
    }
    target.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        target.pod.is_none()
            && target.resource_binding.is_none()
            && options.restart_template.is_none()
            && options.startup_mode == target.builder_startup_mode,
        "Missing builder startup has conflicting physical or restart intent"
    );
    ensure!(
        super::policy::is_kubernetes_builder_lease(receipt),
        "Missing retained-volume compute startup requires its Kubernetes builder lease"
    );
    receipt.validate().map_err(anyhow::Error::msg)?;
    let volumes = retained_volumes(checkpoint)?;
    runtime
        .ensure_absent_builder_control(target, &volumes, receipt)
        .await
        .map_err(Into::into)
}

pub(super) fn retained_volumes(
    checkpoint: &serde_json::Value,
) -> Result<Vec<shared_types::AppResourceIdentity>> {
    let volumes: Vec<shared_types::AppResourceIdentity> =
        serde_json::from_value(checkpoint.get("builder_volumes").cloned().ok_or_else(|| {
            anyhow::anyhow!("Missing builder retained-volume witness unavailable")
        })?)?;
    ensure!(
        !volumes.is_empty()
            && volumes
                .iter()
                .all(|volume| super::policy::is_pvc(volume.kind)
                    && !volume.name.is_empty()
                    && !volume.uid.is_empty())
            && volumes
                .iter()
                .map(|volume| &volume.name)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == volumes.len()
            && volumes
                .iter()
                .map(|volume| &volume.uid)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == volumes.len(),
        "Missing builder requires unique actual retained PVC identities"
    );
    Ok(volumes)
}

/// Observe current compute and data twice before discarding an old recipe.
/// Namespace and PVC ownership are checked by the existing runtime capture.
pub(super) async fn confirm_missing_target<R: AgentContainerRuntime + ?Sized>(
    runtime: &R,
    target: &BuilderControlTarget,
    checkpoint: &serde_json::Value,
) -> Result<()> {
    target.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        target.workload.is_none() && target.pod.is_none() && target.resource_binding.is_none(),
        "Missing builder has a conflicting physical resource"
    );
    let expected = retained_volumes(checkpoint)?;
    for _ in 0..2 {
        let mut actual = runtime
            .capture_bound_builder_control(&target.context, None)
            .await?;
        actual.restart_image = target.restart_image.clone();
        actual.restart_runtime_workspace = target.restart_runtime_workspace.clone();
        actual.builder_startup_mode = target.builder_startup_mode;
        ensure!(
            actual == *target,
            "A builder appeared while confirming retained compute absence"
        );
        ensure!(
            runtime.capture_builder_compute_volumes(&actual).await? == expected,
            "Missing builder's original retained PVC changed"
        );
    }
    Ok(())
}

/// Convert a stopped vanished Kubernetes controller to current compute absence.
/// The caller supplies its durable Stopped boundary; unknown Starting is never
/// eligible. The current factory derives the same canonical source directory.
pub(super) async fn prepare_missing_restart_target<
    R: AgentContainerRuntime + UserAppDeploymentRuntime + ?Sized,
>(
    runtime: &R,
    original: &BuilderControlTarget,
    observed: &BuilderControlTarget,
    checkpoint: &serde_json::Value,
    receipt: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<BuilderControlTarget> {
    original.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        original
            .workload
            .as_ref()
            .is_some_and(|workload| super::policy::uses_retained_pvc(workload.kind))
            && observed.context == original.context
            && observed.workload.is_none()
            && observed.pod.is_none()
            && observed.resource_binding.is_none()
            && original
                .restart_runtime_workspace
                .as_ref()
                .is_none_or(|workspace| workspace
                    == &shared_types::paths::userapp_dev_workspace(&original.context.app_id)),
        "Vanished builder cannot change its original application or source directory"
    );
    ensure!(
        super::policy::is_kubernetes_builder_lease(receipt),
        "Vanished builder recreation requires its original Kubernetes compute lease"
    );
    receipt.validate().map_err(anyhow::Error::msg)?;
    ensure!(
        runtime
            .validate_app_operation_receipt(&original.context, receipt)
            .await?,
        "Vanished builder's original lease is not held"
    );
    let mut target = observed.clone();
    target.restart_image = original.restart_image.clone();
    target.restart_runtime_workspace = None;
    target.builder_startup_mode = original.builder_startup_mode;
    confirm_missing_target(runtime, &target, checkpoint).await?;
    ensure!(
        runtime
            .validate_app_operation_receipt(&original.context, receipt)
            .await?,
        "Vanished builder's original lease changed during absence verification"
    );
    Ok(target)
}

/// Preserve opaque history without sending it to archive restore or cleanup.
pub(super) fn retain_template_history(
    original: &serde_json::Value,
    prepared: &mut serde_json::Value,
) -> Result<()> {
    if let Some(history) = original.get("builder_restart_template_history") {
        prepared["builder_restart_template_history"] = history.clone();
    }
    if let Some(template) = original.get("builder_restart_template") {
        let history = prepared
            .as_object_mut()
            .context("Builder checkpoint is not an object")?
            .entry("builder_restart_template_history")
            .or_insert_with(|| serde_json::json!([]));
        if let Some(history) = history.as_array_mut() {
            history.push(template.clone());
        } else {
            // Preserve even malformed historical diagnostics without decoding them.
            *history = serde_json::json!([history.clone(), template.clone()]);
        }
    }
    Ok(())
}

/// Classify an actual missing-create boundary, never a history-only flag.
pub(super) fn missing_creation(record: &shared_types::ComputeControlRecord) -> bool {
    let Ok(target) = serde_json::from_value::<BuilderControlTarget>(record.checkpoint.clone())
    else {
        return false;
    };
    let builder_scope = match record.scope {
        shared_types::UserAppOperationScope::Dev => true,
        shared_types::UserAppOperationScope::Prod
        | shared_types::UserAppOperationScope::Application => false,
    };
    builder_scope
        && super::policy::is_restart(record.action)
        && record
            .lease
            .as_ref()
            .is_some_and(super::policy::is_kubernetes_builder_lease)
        && record
            .lease
            .as_ref()
            .is_some_and(|receipt| receipt.validate().is_ok())
        && record
            .execution_context()
            .is_ok_and(|context| context == target.context)
        && target.validate().is_ok()
        && target.workload.is_none()
        && target.pod.is_none()
        && target.resource_binding.is_none()
        && retained_volumes(&record.checkpoint).is_ok()
}

#[cfg(test)]
mod tests;
