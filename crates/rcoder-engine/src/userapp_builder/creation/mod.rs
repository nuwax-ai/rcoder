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

// An optimization only. Database operation state remains authoritative across replicas.
static SIGNALS: LazyLock<Mutex<HashMap<String, watch::Sender<u64>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) async fn ensure(
    state: &AppState,
    app_id: &str,
    instance: &str,
    lease: OwnedMutexGuard<()>,
    deadline: Instant,
) -> Result<ContainerBasicInfo> {
    let flight = state.userapp_op_flight.guard()?;
    let app = state.userapp_store.ensure_identity(app_id).await?;
    let fingerprint = instance_fingerprint(state, app_id)?;
    let admission = state
        .userapp_store
        .admit(&UserAppAdmission {
            runtime_policy_on_success: None,
            command: None,
            metadata: None,
            app_id: app_id.into(),
            lifecycle_id: Some(app.lifecycle_id),
            operation_id: uuid::Uuid::new_v4().to_string(),
            request_id: None,
            request_fingerprint: fingerprint,
            kind: UserAppOperationKind::EnsureBuilder,
        })
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

/// builder 创建指纹：键含 app_id 与平台配置——同 app 重创建指纹稳定，
/// 配置变更即变。schema 3 起 user 维度已移除（应用共享，无实例 user）；
/// 与复合键时代（schema 2）指纹不兼容，存量 pending 恢复会判
/// configuration_changed 转 RecoveryRequired（存量重建语义，已拍板）。
pub(super) fn instance_fingerprint(state: &AppState, app_id: &str) -> Result<String> {
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

// 拆分（file-server 大文件范式）：`spawn` 受理派发与迟到结果 /
// `fence` 围栏证据 / `observe` 就绪观察与等待收敛；两个测试模块独立成文件
// （`fence_settler_tests` 被 recovery.rs 以 `creation::fence_settler_tests`
// 路径引用，须保持可达）。旧路径 `creation::X` 经 glob 重导出保持不变。

mod fence;
#[cfg(test)]
pub(crate) mod fence_settler_tests;
mod observe;
mod spawn;
#[cfg(test)]
mod tests;

pub(super) use fence::*;
pub(super) use observe::*;
pub(super) use spawn::*;
