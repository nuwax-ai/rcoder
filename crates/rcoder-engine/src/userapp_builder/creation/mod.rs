//! Durable builder admission. Cancelling a waiter never drops the execution lease.
use crate::app_state::AppState;
use anyhow::{Context, Result, anyhow};
use shared_types::{
    ContainerBasicInfo, UserAppAdmission, UserAppAdmissionOutcome, UserAppLifecycleStore,
    UserAppOperationKind, UserAppOperationProgress, UserAppOperationRecord, UserAppOperationState,
};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::{
    sync::{OwnedMutexGuard, watch},
    time::Instant,
};

// Read-only runtime observations retain the existing per-call recovery budget.
const RUNTIME_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);

// An optimization only. Database operation state remains authoritative across replicas.
// hotpath 锁观测（T2.3 仅此登记表选点）：mutex! 关闭时原样返回原生 std::sync::Mutex，
// 开启时包装记录锁等待（wait）与持锁（acquire）时长；lock/poison 语义与调用点不变，
// 锁作用域与 lease 所有权不因观测改变（OwnedMutexGuard/DashMap 不接，见方案 D2-lock）。
use hotpath::wrap::std::sync::Mutex as ObsMutex;
static SIGNALS: LazyLock<ObsMutex<HashMap<String, watch::Sender<u64>>>> = LazyLock::new(|| {
    hotpath::mutex!(
        Mutex::new(HashMap::new()),
        // Keep the safe expression-label API; literal labels export symbols.
        label = { "userapp_builder_signals" }
    )
});

pub(super) async fn ensure(
    state: &AppState,
    app_id: &str,
    instance: &str,
    lease: OwnedMutexGuard<()>,
    deadline: Instant,
    startup_mode: shared_types::BuilderStartupMode,
) -> Result<ContainerBasicInfo> {
    let flight = state.userapp_op_flight.guard()?;
    let app = match super::adoption::discover_missing_identity(state, app_id).await? {
        Some(app) => app,
        None => state.userapp_store.ensure_identity(app_id).await?,
    };
    let input = creation_input(state, app_id, startup_mode)?;
    let admission = state
        .userapp_store
        .admit_with_input(
            &UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: app_id.into(),
                lifecycle_id: Some(app.lifecycle_id),
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: None,
                request_fingerprint: input.digest(),
                kind: UserAppOperationKind::EnsureBuilder,
            },
            Some(&input),
        )
        .await?;
    let operation = match admission {
        UserAppAdmissionOutcome::Accepted(record) => {
            // Detach only the HTTP waiter. The worker retains its lease and
            // commits its outcome even when this request is cancelled.
            drop(spawn_operation(state, &record, instance, lease, flight)?);
            record
        }
        UserAppAdmissionOutcome::Existing(record) => {
            drop(lease);
            record
        }
    };
    wait(state, &operation, instance, deadline).await
}

/// New private input contains no environment values or unrelated platform
/// secrets. Its digest binds startup intent to the builder configuration.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BuilderCreationInput {
    schema: u8,
    app_id: String,
    configuration_fingerprint: String,
    startup_mode: shared_types::BuilderStartupMode,
}

fn creation_input(
    state: &AppState,
    app_id: &str,
    startup_mode: shared_types::BuilderStartupMode,
) -> Result<shared_types::UserAppExecutionInput> {
    use sha2::{Digest, Sha256};
    let docker = state.config.docker_config.as_ref().and_then(|config| {
        config
            .get_multi_image_config()
            .get_service_config(&shared_types::ServiceType::UserappBuilder)
            .cloned()
    });
    let kubernetes =
        state.config.kubernetes_config.as_ref().and_then(|config| {
            config.get_service_config(&shared_types::ServiceType::UserappBuilder)
        });
    let configuration = serde_json::json!({
        "storage": super::DEFAULT_BUILDER_STORAGE_SIZE,
        "docker": docker,
        "kubernetes": kubernetes,
    });
    let input = BuilderCreationInput {
        schema: 4,
        app_id: app_id.to_owned(),
        configuration_fingerprint: hex::encode(Sha256::digest(
            shared_types::encode_userapp_intent(&configuration)?,
        )),
        startup_mode,
    };
    Ok(shared_types::UserAppExecutionInput::new(
        serde_json::to_string(&input)?,
    ))
}

fn pending_configuration_matches(
    state: &AppState,
    operation: &UserAppOperationRecord,
) -> Result<bool> {
    for mode in [
        shared_types::BuilderStartupMode::ManagementOnly,
        shared_types::BuilderStartupMode::PreserveBusinessIntent,
    ] {
        if creation_input(state, &operation.app_id, mode)?.digest() == operation.request_fingerprint
        {
            return Ok(true);
        }
    }
    Ok(legacy_instance_fingerprint(state, &operation.app_id)? == operation.request_fingerprint)
}

/// Legacy Pending records have no private mode input. Preserve their exact
/// original startup behavior, rather than interpreting them as new requests.
fn legacy_instance_fingerprint(state: &AppState, app_id: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    let config = serde_json::json!({"schema":3,"app_id":app_id,
        "storage":super::DEFAULT_BUILDER_STORAGE_SIZE,
        "docker":state.config.docker_config,"kubernetes":state.config.kubernetes_config});
    Ok(
        Sha256::digest(shared_types::encode_userapp_intent(&config)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

async fn claimed_startup_mode(
    state: &AppState,
    operation: &UserAppOperationRecord,
    executor_id: &str,
) -> Result<shared_types::BuilderStartupMode> {
    if operation.request_fingerprint == legacy_instance_fingerprint(state, &operation.app_id)? {
        return Ok(shared_types::BuilderStartupMode::PreserveBusinessIntent);
    }
    let context = shared_types::UserAppExecutionContext {
        app_id: operation.app_id.clone(),
        lifecycle_id: operation.lifecycle_id.clone(),
        operation_id: operation.operation_id.clone(),
        executor_id: executor_id.to_owned(),
        request_fingerprint: operation.request_fingerprint.clone(),
    };
    let input = state.userapp_store.read_execution_input(&context).await?;
    let input: BuilderCreationInput =
        serde_json::from_str(input.encoded()).context("Read original builder startup intent")?;
    if input.schema != 4 || input.app_id != operation.app_id {
        return Err(anyhow!(
            "Builder creation input belongs to another application or schema"
        ));
    }
    Ok(input.startup_mode)
}

pub(super) async fn captured_storage(
    state: &AppState,
    context: &shared_types::UserAppExecutionContext,
) -> Result<Option<shared_types::UserAppRetainedStorage>> {
    let operation = state
        .userapp_store
        .get_operation(&context.app_id, &context.operation_id)
        .await?
        .ok_or_else(|| anyhow!("Builder operation missing"))?;
    operation
        .checkpoint
        .get("retained_storage")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(Into::into)
}

// 拆分（file-server 大文件范式）：`spawn` 受理派发与迟到结果 /
// `fence` 围栏证据 / `observe` 就绪观察与等待收敛；两个测试模块独立成文件
// （`fence_settler_tests` 被 recovery.rs 以 `creation::fence_settler_tests`
// 路径引用，须保持可达）。旧路径 `creation::X` 经 glob 重导出保持不变。

mod fence;
#[cfg(test)]
pub(crate) mod fence_settler_tests;
mod observe;
mod registration;
mod spawn;
#[cfg(test)]
mod tests;

pub(super) use fence::*;
pub(super) use observe::*;
pub(super) use registration::{BuilderRegistrationConfirmationRequired, repair_live_registration};
pub(super) use spawn::*;
