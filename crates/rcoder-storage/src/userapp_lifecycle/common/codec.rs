//! Checked conversion between storage columns and public domain records.
//! Unknown states/versions are errors, never an empty active-operation slot.
use crate::db::models;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use shared_types::{
    UserAppActiveOperations, UserAppLifecycleRecord, UserAppLifecycleState, UserAppOperationKind,
    UserAppOperationRecord, UserAppOperationScope, UserAppOperationState, UserAppRuntimePolicy,
};

macro_rules! codec {
    ($encode:ident, $decode:ident, $ty:ident, {$($variant:ident => $wire:literal),+ $(,)?}) => {
        pub(super) fn $encode(value: $ty) -> &'static str { match value { $($ty::$variant => $wire),+ } }
        pub(super) fn $decode(value: &str) -> Result<$ty> {
            match value { $($wire => Ok($ty::$variant),)+ _ => anyhow::bail!(concat!("unknown persisted ", stringify!($ty))) }
        }
    }
}
codec!(lifecycle_name, lifecycle, UserAppLifecycleState, {
    Active => "active", Deleting => "deleting", Deleted => "deleted"
});
codec!(scope_name, scope, UserAppOperationScope, {
    Dev => "dev", Prod => "prod", Application => "application"
});
codec!(state_name, state, UserAppOperationState, {
    Pending => "pending", Running => "running", WaitingRetry => "waiting_retry",
    RecoveryRequired => "recovery_required", Succeeded => "succeeded", Failed => "failed"
});
codec!(kind_name, kind, UserAppOperationKind, {
    EnsureBuilder => "ensure_builder", AdoptBuilder => "adopt_builder",
    StopBuilder => "stop_builder", RestartBuilder => "restart_builder",
    Create => "create", StartDeployment => "start_deployment", RestartDeployment => "restart_deployment",
    Update => "update", Start => "start", Restart => "restart", Stop => "stop",
    SetRecyclePolicy => "set_recycle_policy", HotDeploy => "hot_deploy", DeleteCompute => "delete_compute",
    PurgeResources => "purge_resources", DestroyDevStorage => "destroy_dev_storage",
    DestroyProdStorage => "destroy_prod_storage", ClearDevStorage => "clear_dev_storage",
    ResetDevDatabasePassword => "reset_dev_database_password",
    ResetProdDatabasePassword => "reset_prod_database_password",
    PrepareProdDatabase => "prepare_prod_database",
    ClearProdStorage => "clear_prod_storage", DeleteApplication => "delete_application"
});
fn timestamp(value: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_micros(value)
        .context("persisted UTC microsecond timestamp is out of range")
}
fn positive(value: i64) -> Result<i64> {
    ensure!(value >= 1, "persisted revision or epoch must be positive");
    Ok(value)
}
fn nonempty(value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "persisted identity is empty");
    Ok(())
}

pub(super) fn application(
    row: models::Application,
    slots: models::ActiveOperations,
) -> Result<UserAppLifecycleRecord> {
    nonempty(&row.app_id)?;
    nonempty(&row.lifecycle_id)?;
    ensure!(
        row.app_id == slots.app_id && row.lifecycle_id == slots.lifecycle_id,
        "active operation slots belong to a different lifecycle"
    );
    ensure!(
        slots.application_operation_id.is_none()
            || (slots.dev_operation_id.is_none() && slots.prod_operation_id.is_none()),
        "application operation overlaps an environment operation"
    );
    if let (Some(dev), Some(prod)) = (&slots.dev_operation_id, &slots.prod_operation_id) {
        ensure!(
            dev != prod,
            "same operation occupies both environment slots"
        );
    }
    for id in [
        &slots.dev_operation_id,
        &slots.prod_operation_id,
        &slots.application_operation_id,
    ]
    .into_iter()
    .flatten()
    {
        nonempty(id)?;
    }
    Ok(UserAppLifecycleRecord {
        app_id: row.app_id,
        lifecycle_id: row.lifecycle_id,
        lifecycle_epoch: positive(row.lifecycle_epoch)?,
        metadata_revision: positive(row.metadata_revision)?,
        state: lifecycle(&row.lifecycle_state)?,
        name: row.name,
        tenant_id: row.tenant_id,
        space_id: row.space_id,
        created_at: timestamp(row.created_at_us)?,
        runtime_policy: UserAppRuntimePolicy {
            recycle_enabled: row.recycle_enabled,
            wake_on_traffic: row.wake_on_traffic,
            idle_timeout_seconds: row
                .idle_timeout_seconds
                .map(u64::try_from)
                .transpose()
                .context("negative persisted idle timeout")?,
        },
        active_operations: UserAppActiveOperations {
            dev: slots.dev_operation_id,
            prod: slots.prod_operation_id,
            application: slots.application_operation_id,
        },
    })
}

pub(super) fn application_row(
    record: &UserAppLifecycleRecord,
    now_us: i64,
) -> Result<models::Application> {
    Ok(models::Application {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        lifecycle_epoch: positive(record.lifecycle_epoch)?,
        metadata_revision: positive(record.metadata_revision)?,
        lifecycle_state: lifecycle_name(record.state).into(),
        name: record.name.clone(),
        tenant_id: record.tenant_id.clone(),
        space_id: record.space_id.clone(),
        recycle_enabled: record.runtime_policy.recycle_enabled,
        wake_on_traffic: record.runtime_policy.wake_on_traffic,
        idle_timeout_seconds: record
            .runtime_policy
            .idle_timeout_seconds
            .map(i64::try_from)
            .transpose()
            .context("idle timeout exceeds database integer range")?,
        created_at_us: record.created_at.timestamp_micros(),
        updated_at_us: now_us,
    })
}

pub(super) fn operation(row: models::Operation) -> Result<UserAppOperationRecord> {
    ensure!(
        row.payload_version == 1,
        "unsupported operation payload version"
    );
    for id in [&row.operation_id, &row.app_id, &row.lifecycle_id] {
        nonempty(id)?;
    }
    let state = state(&row.state)?;
    let kind = kind(&row.kind)?;
    let scope = scope(&row.scope)?;
    ensure!(
        kind.scope() == scope,
        "persisted operation kind/scope mismatch"
    );
    ensure!(
        state.is_terminal() == row.terminal_at_us.is_some(),
        "persisted operation terminal marker mismatch"
    );
    if state == UserAppOperationState::Running {
        ensure!(
            row.executor_id.as_ref().is_some_and(|id| !id.is_empty()),
            "running operation is missing executor identity"
        );
    }
    ensure!(
        row.request_fingerprint.len() == 64
            && row
                .request_fingerprint
                .bytes()
                .all(|b| b.is_ascii_hexdigit()),
        "invalid persisted request fingerprint"
    );
    let command = row
        .command_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .context("decode operation command payload")?;
    let admitted_metadata = row
        .admitted_metadata_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .context("decode admitted metadata payload")?;
    let runtime_policy_on_success = row
        .runtime_policy_on_success_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .context("decode runtime policy payload")?;
    Ok(UserAppOperationRecord {
        operation_id: row.operation_id,
        app_id: row.app_id,
        lifecycle_id: row.lifecycle_id,
        request_id: row.origin_request_id,
        request_fingerprint: row.request_fingerprint,
        kind,
        scope,
        state,
        revision: positive(row.revision)?,
        executor_id: row.executor_id,
        step: row.step,
        checkpoint: serde_json::from_str(&row.checkpoint_json)
            .context("decode operation checkpoint payload")?,
        error_code: row.error_code,
        error_message: row.error_message,
        created_at: timestamp(row.created_at_us)?,
        command,
        admitted_metadata,
        runtime_policy_on_success,
    })
}

pub(super) fn operation_row(
    record: &UserAppOperationRecord,
    now_us: i64,
    previous_terminal_us: Option<i64>,
) -> Result<models::Operation> {
    ensure!(
        record.kind.scope() == record.scope,
        "operation kind/scope mismatch"
    );
    Ok(models::Operation {
        operation_id: record.operation_id.clone(),
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        kind: kind_name(record.kind).into(),
        scope: scope_name(record.scope).into(),
        state: state_name(record.state).into(),
        revision: positive(record.revision)?,
        origin_request_id: record.request_id.clone(),
        request_fingerprint: record.request_fingerprint.clone(),
        executor_id: record.executor_id.clone(),
        step: record.step.clone(),
        error_code: record.error_code.clone(),
        error_message: record.error_message.clone(),
        payload_version: 1,
        command_json: record
            .command
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
        admitted_metadata_json: record
            .admitted_metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
        runtime_policy_on_success_json: record
            .runtime_policy_on_success
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?,
        checkpoint_json: serde_json::to_string(&record.checkpoint)?,
        created_at_us: record.created_at.timestamp_micros(),
        updated_at_us: now_us,
        terminal_at_us: record
            .state
            .is_terminal()
            .then(|| previous_terminal_us.unwrap_or(now_us)),
    })
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
