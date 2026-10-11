//! Retain the operation's private restart snapshot at every startup entrypoint.
#[cfg(test)]
use anyhow::Context as _;
use anyhow::{Result, ensure};
use container_runtime_api::AgentContainerRuntime;
#[cfg(test)]
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
    let template = match checkpoint.get("builder_restart_template") {
        None => None,
        Some(saved) => match serde_json::from_value::<BuilderRestartTemplate>(saved.clone()) {
            Ok(template) if template.source.context == target.context => Some(template),
            _ => {
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

#[cfg(test)]
mod tests;
