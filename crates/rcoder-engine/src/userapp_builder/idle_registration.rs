//! A controller-created Pod successor is not a new workload adoption. Repair
//! only its registration, never stop it using the predecessor's idle evidence.
use super::{adoption, lifecycle, refreshed_registration, validate_builder_identity};
use crate::app_state::AppState;
use anyhow::{Context, Result, ensure};
use rcoder_storage::WorkloadAdmissionMode;
use shared_types::{
    ProjectAndContainerInfo, ServiceType, UserAppOperationScope,
    persistence::PersistenceWriteOutcome,
};
use std::sync::Arc;

/// true means this scan must stop using its old project/physical snapshot.
/// No compute, lifecycle, binding, operation receipt or workspace is created.
pub(crate) async fn refresh_idle_builder_pod(
    state: &AppState,
    expected: &ProjectAndContainerInfo,
) -> Result<bool> {
    let budget = std::time::Duration::from_secs(
        state
            .config
            .cleanup_config
            .orphan_scan_budget_seconds
            .max(1),
    );
    tokio::time::timeout(budget, refresh_idle_builder_pod_inner(state, expected))
        .await
        .context("Builder idle registration observation remains unconfirmed")?
}

async fn refresh_idle_builder_pod_inner(
    state: &AppState,
    expected: &ProjectAndContainerInfo,
) -> Result<bool> {
    if expected.service_type() != Some(ServiceType::UserappBuilder) {
        return Ok(false);
    }
    let app_id = expected.project_id();
    let Some(previous) = expected.container_info() else {
        return Ok(false);
    };
    let expected_generation = expected.persistence_identity().generation.clone();
    let Some(observed) = state
        .runtime()
        .find_container(app_id, &ServiceType::UserappBuilder)
        .await?
    else {
        return Ok(false);
    };
    if observed.container_id == previous.container_id {
        return Ok(false);
    }
    validate_builder_identity(app_id, &observed)?;
    ensure!(
        previous
            .workload_uid
            .as_ref()
            .is_some_and(|uid| !uid.is_empty())
            && observed.workload_uid == previous.workload_uid,
        "Builder workload changed; idle observation cannot authorize adoption"
    );
    let guard = state
        .acquire_agent_workload(
            app_id,
            &ServiceType::UserappBuilder,
            WorkloadAdmissionMode::Exclusive,
        )
        .await?;
    let Some(_local) = lifecycle::try_acquire(app_id).await else {
        return Ok(true);
    };
    state
        .userapp_store
        .check_compute_access(app_id, UserAppOperationScope::Dev, false)
        .await?;
    if crate::cleanup_task::cleaner::builder_fenced_by_active_operations(
        state.userapp_store.as_ref(),
        app_id,
    )
    .await
    {
        return Ok(true);
    }
    let Some(current) = state.projects.refresh_project_registration(app_id).await? else {
        return Ok(true);
    };
    let Some(canonical) = current.container_info() else {
        return Ok(true);
    };
    if current.persistence_identity().generation != expected_generation
        || canonical.container_id != previous.container_id
        || canonical.workload_uid != previous.workload_uid
    {
        return Ok(true);
    }
    let frame = state
        .projects
        .capture_workload_registration(&guard, guard.key().logical_name())
        .await?;
    ensure!(
        frame.project_ids() == vec![app_id]
            && frame.physical_uid() == Some(previous.container_id.as_str()),
        "Builder registration has different or shared canonical references"
    );
    let Some(actual) = state
        .runtime()
        .find_container(app_id, &ServiceType::UserappBuilder)
        .await?
    else {
        return Ok(true);
    };
    validate_builder_identity(app_id, &actual)?;
    ensure!(
        actual.workload_uid == previous.workload_uid
            && actual.container_id == observed.container_id
            && actual.container_name == previous.container_name,
        "Builder changed while refreshing its idle registration"
    );
    adoption::verify_live_builder_observation(state, app_id, app_id, &actual).await?;
    let Some(updated) = refreshed_registration(&canonical, &actual) else {
        return Ok(true);
    };
    state
        .userapp_store
        .check_compute_access(app_id, UserAppOperationScope::Dev, false)
        .await?;
    guard.verify_current().await?;
    let mut project = (*current).clone();
    project.set_container(Some(updated));
    // Activity/grace belongs to the new execution. Old idle evidence must never
    // cause its immediate retirement, even when the old project was very old.
    project.update_activity();
    let outcome = state
        .projects
        .insert_project_registration_durable(app_id.into(), Arc::new(project))
        .await?;
    match outcome {
        PersistenceWriteOutcome::Committed => tracing::info!(app_id,
            old_pod = %previous.container_id, current_pod = %actual.container_id,
            "Builder Pod registration refreshed; successor starts a new idle observation window"),
        PersistenceWriteOutcome::Superseded => {}
        PersistenceWriteOutcome::Deferred { reason } => {
            anyhow::bail!("Builder Pod registration update remains unconfirmed: {reason}")
        }
    }
    Ok(true)
}
