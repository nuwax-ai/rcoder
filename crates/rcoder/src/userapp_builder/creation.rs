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
    owner: &str,
    lease: OwnedMutexGuard<()>,
    deadline: Instant,
) -> Result<ContainerBasicInfo> {
    let app = state.userapp_store.ensure_identity(app_id, owner).await?;
    let fingerprint = instance_fingerprint(state, app_id, instance, owner)?;
    let admission = state
        .userapp_store
        .admit(&UserAppAdmission {
            runtime_policy_on_success: None,
            command: None,
            metadata: None,
            app_id: app_id.into(),
            user_id: owner.into(),
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
            drop(spawn_operation(state, &record, instance, owner, lease)?);
            record
        }
        UserAppAdmissionOutcome::Existing(record) => {
            drop(lease);
            record
        }
    };
    wait(state, &operation, instance, deadline).await
}

/// builder 实例创建指纹（owner 受理路径与协作者轻量路径同源）：键含复合
/// identifier 与实例 user——同实例重创建指纹稳定，换实例/换 user 即变。
/// 注意：与复合键化之前的指纹（纯 app_id 键）不兼容，升级后存量 pending
/// 恢复会判 configuration_changed 转 RecoveryRequired（存量重建语义，已拍板）。
pub(super) fn instance_fingerprint(
    state: &AppState,
    app_id: &str,
    instance: &str,
    instance_user: &str,
) -> Result<String> {
    use sha2::{Digest, Sha256};
    let config = serde_json::json!({"schema":2,"app_id":app_id,"instance":instance,
        "user_id":instance_user,
        "storage":super::DEFAULT_BUILDER_STORAGE_SIZE,
        "docker":state.config.docker_config,"kubernetes":state.config.kubernetes_config});
    Ok(
        Sha256::digest(shared_types::encode_userapp_intent(&config)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

fn spawn_operation(
    state: &AppState,
    record: &UserAppOperationRecord,
    instance: &str,
    owner: &str,
    lease: OwnedMutexGuard<()>,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let (signal, _) = watch::channel(0);
    SIGNALS
        .lock()
        .map_err(|_| anyhow!("Builder notification registry lock poisoned"))?
        .insert(record.operation_id.clone(), signal.clone());
    let worker_state = state.clone();
    let worker_record = record.clone();
    let owner = owner.to_owned();
    let instance = instance.to_string();
    let worker = tokio::spawn(async move {
        let executor = uuid::Uuid::new_v4().to_string();
        let owned = worker_state.clone();
        let claimed_id = executor.clone();
        let initial = worker_record.clone();
        let execution_signal = signal.clone();
        let execution = tokio::spawn(async move {
            let _lease = lease;
            let claimed = progress(
                &owned.userapp_store,
                &initial,
                &claimed_id,
                UserAppOperationState::Running,
                "claimed",
                serde_json::Value::Null,
                None,
            )
            .await?;
            let ready_deadline = Instant::now()
                + Duration::from_secs(owned.config.userapp_storage.ensure_timeout_seconds);
            let creation = super::create_builder_inner(
                &owned,
                &claimed.app_id,
                &instance,
                &owner,
                shared_types::UserAppExecutionContext {
                    app_id: instance.clone(),
                    user_id: owner.clone(),
                    lifecycle_id: claimed.lifecycle_id.clone(),
                    operation_id: claimed.operation_id.clone(),
                    executor_id: claimed_id.clone(),
                    request_fingerprint: claimed.request_fingerprint.clone(),
                },
            );
            let observed = observe_creation_budget(tokio::time::sleep_until(ready_deadline), creation, async {
                progress(
                    &owned.userapp_store,
                    &claimed,
                    &claimed_id,
                    UserAppOperationState::RecoveryRequired,
                    "creation_confirmation_timed_out",
                    serde_json::Value::Null,
                    Some("Builder confirmation deadline exceeded; remote execution is still being observed".into()),
                )
                .await?;
                execution_signal.send_replace(1);
                Ok(())
            })
            .await?;
            let Some(created) = observed else {
                // A late remote result is evidence for reconciliation, never a
                // new success or a registration published by this old worker.
                return Ok(());
            };
            let result = match created {
                Ok(info) => {
                    super::confirm_builder_ready(
                        &owned,
                        &claimed.app_id,
                        &instance,
                        info,
                        ready_deadline,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            let mut completion = claimed.clone();
            let (status, value, error) = match result {
                Ok(info) => {
                    let context = shared_types::UserAppExecutionContext {
                        // capture 侧按复合 identifier 定位容器（与上方 create_builder_inner
                        // 同源）；claimed.app_id 是 lifecycle 纯 app_id，用它会 inspect
                        // 少 user 段的旧名单 → 404 → workload 缺失（实测踩坑）
                        app_id: instance.clone(), user_id: owner.clone(),
                        lifecycle_id: claimed.lifecycle_id.clone(), operation_id: claimed.operation_id.clone(),
                        executor_id: claimed_id.clone(), request_fingerprint: claimed.request_fingerprint.clone(),
                    };
                    let evidence = shared_types::BuilderCreationEvidence {
                        creation_lease_released: true,
                        target: super::adoption::capture_bound_target(&owned, &context).await?,
                        container: info.clone(),
                    };
                    evidence.validate_operation(&claimed).map_err(anyhow::Error::msg)?;
                    completion = progress(&owned.userapp_store, &claimed, &claimed_id,
                        UserAppOperationState::Running, "builder_ready_confirmed",
                        serde_json::to_value(&evidence)?, None).await?;
                    super::register_builder(&owned, &instance, &info)?;
                    (
                        UserAppOperationState::Succeeded,
                        serde_json::to_value(info)?,
                        None,
                    )
                },
                Err(error) => {
                    let rejected = error
                        .downcast_ref::<container_runtime_api::ContainerRuntimeError>()
                        .is_some_and(|e| {
                            matches!(
                                e,
                                container_runtime_api::ContainerRuntimeError::RequestRejected(_)
                            )
                        });
                    (
                        if rejected {
                            UserAppOperationState::Failed
                        } else {
                            UserAppOperationState::RecoveryRequired
                        },
                        serde_json::Value::Null,
                        Some(format!("{error:#}")),
                    )
                }
            };
            progress(
                &owned.userapp_store,
                &completion,
                &claimed_id,
                status,
                "creation_result",
                value,
                error,
            )
            .await?;
            Ok::<(), anyhow::Error>(())
        })
        .await;
        if !matches!(&execution, Ok(Ok(()))) {
            // A panic or a failed checkpoint is not proof that runtime creation failed.
            if let Ok(Some(current)) = worker_state
                .userapp_store
                .get_operation(&worker_record.app_id, &worker_record.operation_id)
                .await
                && current.state == UserAppOperationState::Running
                && current.executor_id.as_deref() == Some(&executor)
                && let Err(error) = progress(
                    &worker_state.userapp_store,
                    &current,
                    &executor,
                    UserAppOperationState::RecoveryRequired,
                    if shared_types::userapp_operation_has_final_evidence(&current) {
                        &current.step
                    } else {
                        "worker_interrupted"
                    },
                    current.checkpoint.clone(),
                    Some("Builder worker did not commit its result".into()),
                )
                .await
            {
                tracing::error!(%error, operation_id = %current.operation_id, "Record interrupted builder operation failed");
            }
            tracing::error!(operation_id = %worker_record.operation_id, result = ?execution, "Builder operation requires reconciliation");
        }
        signal.send_replace(1);
        if let Ok(mut signals) = SIGNALS.lock() {
            signals.remove(&worker_record.operation_id);
        }
        match execution {
            Ok(result) => result,
            Err(error) => Err(anyhow!(error).context("Builder execution task interrupted")),
        }
    });
    Ok(worker)
}

/// Bound confirmation, not remote execution. Runtime implementations own a
/// cancellation-safe task and lease; dropping their future would detach that
/// task. Keep observing it after recording the deadline, retaining our local
/// lease until its actual outcome is available.
async fn observe_creation_budget<T>(
    deadline: impl Future<Output = ()>,
    creation: impl Future<Output = Result<T>>,
    expired: impl Future<Output = Result<()>>,
) -> Result<Option<Result<T>>> {
    tokio::pin!(creation);
    let started = std::sync::atomic::AtomicBool::new(false);
    let observed = std::future::poll_fn(|cx| {
        started.store(true, std::sync::atomic::Ordering::Relaxed);
        Future::poll(creation.as_mut(), cx)
    });
    tokio::pin!(observed);
    tokio::select! {
        biased;
        _ = deadline => {
            let recorded = expired.await;
            // Never start a fresh write after expiry. Once polled, however,
            // even a failed status write must not detach the in-flight task.
            if started.load(std::sync::atomic::Ordering::Relaxed) {
                let outcome = observed.await;
                tracing::warn!(succeeded = outcome.is_ok(), "Observed builder result after confirmation deadline; reconciliation is required");
            }
            recorded?;
            Ok(None)
        }
        result = &mut observed => Ok(Some(result)),
    }
}

pub(super) async fn resume_pending(
    state: &AppState,
    pending: &UserAppOperationRecord,
) -> Result<bool> {
    if pending.kind != UserAppOperationKind::EnsureBuilder
        || pending.state != UserAppOperationState::Pending
    {
        return Ok(false);
    }
    let Some(lease) = super::lifecycle::try_acquire(&pending.app_id).await else {
        return Ok(false);
    };
    let current = state
        .userapp_store
        .get_operation(&pending.app_id, &pending.operation_id)
        .await?
        .ok_or_else(|| anyhow!("Pending operation disappeared during recovery"))?;
    if current.state != UserAppOperationState::Pending || current.revision != pending.revision {
        return Ok(false);
    }
    let identity = state
        .userapp_store
        .get_application(&current.app_id)
        .await?
        .ok_or_else(|| anyhow!("Pending builder has no application identity"))?;
    if identity.lifecycle_id != current.lifecycle_id
        || identity.state != shared_types::UserAppLifecycleState::Active
    {
        return Err(anyhow!(
            "Pending builder lifecycle does not match recovery target"
        ));
    }
    // 恢复指纹按 owner 实例复合键重算（record/identity 不含实例段——owner
    // 受理操作的实例恒为 {identity.user_id}-{app_id}）
    let instance = shared_types::builder_instance_id(&identity.user_id, &current.app_id)
        .map_err(anyhow::Error::msg)?;
    if instance_fingerprint(state, &current.app_id, &instance, &identity.user_id)?
        != current.request_fingerprint
    {
        let executor = uuid::Uuid::new_v4().to_string();
        let claimed = progress(
            &state.userapp_store,
            &current,
            &executor,
            UserAppOperationState::Running,
            "claimed",
            serde_json::Value::Null,
            None,
        )
        .await?;
        progress(
            &state.userapp_store,
            &claimed,
            &executor,
            UserAppOperationState::RecoveryRequired,
            "configuration_changed",
            serde_json::Value::Null,
            Some(
                "Builder configuration changed before execution; explicit recovery is required"
                    .into(),
            ),
        )
        .await?;
        return Ok(true);
    }
    // Pending is unclaimed; the SQL revision CAS elects exactly one executor.
    // Running and uncertain operations never enter this automatic replay path.
    // Occupy the recovery scheduler slot until the actual worker finishes,
    // including its final checkpoint and notification cleanup. Dropping this
    // observer still detaches rather than aborting the admitted operation.
    spawn_operation(state, &current, &instance, &identity.user_id, lease)?
        .await
        .context("Observe recovered builder worker")??;
    Ok(true)
}

async fn progress(
    store: &Arc<dyn UserAppLifecycleStore>,
    record: &UserAppOperationRecord,
    executor: &str,
    state: UserAppOperationState,
    step: &str,
    checkpoint: serde_json::Value,
    error: Option<String>,
) -> Result<UserAppOperationRecord> {
    Ok(store
        .advance(&UserAppOperationProgress {
            app_id: record.app_id.clone(),
            lifecycle_id: record.lifecycle_id.clone(),
            operation_id: record.operation_id.clone(),
            expected_revision: record.revision,
            executor_id: executor.into(),
            state,
            step: step.into(),
            checkpoint,
            error_code: error.as_ref().map(|_| "ERR_BACKEND_ERROR".into()),
            error_message: error,
        })
        .await?)
}

async fn wait(
    state: &AppState,
    accepted: &UserAppOperationRecord,
    instance: &str,
    deadline: Instant,
) -> Result<ContainerBasicInfo> {
    let operation = wait_record(&state.userapp_store, accepted, deadline).await?;
    let identity = state
        .userapp_store
        .get_application(&accepted.app_id)
        .await?
        .ok_or_else(|| anyhow!("Builder lifecycle disappeared"))?;
    if identity.lifecycle_id != accepted.lifecycle_id
        || identity.state != shared_types::UserAppLifecycleState::Active
    {
        return Err(anyhow!(
            "Builder lifecycle changed before returning its address"
        ));
    }
    let info: ContainerBasicInfo = serde_json::from_value(operation.checkpoint)
        .context("decode completed builder resource identity")?;
    let verified = super::cross_verify_registration(state, &accepted.app_id, instance, &info)
        .await?
        .ok_or_else(|| anyhow!("Completed builder is no longer running"))?;
    if verified.container_id != info.container_id {
        return Err(anyhow!("Completed builder resource was replaced"));
    }
    Ok(verified)
}

async fn wait_record(
    store: &Arc<dyn UserAppLifecycleStore>,
    accepted: &UserAppOperationRecord,
    deadline: Instant,
) -> Result<UserAppOperationRecord> {
    let mut signal = SIGNALS
        .lock()
        .map_err(|_| anyhow!("Builder notification registry lock poisoned"))?
        .get(&accepted.operation_id)
        .map(watch::Sender::subscribe);
    let mut delay = Duration::from_millis(200);
    // Per-waiter jitter avoids a synchronized poll storm across replicas.
    let jitter = Duration::from_millis(u64::from(uuid::Uuid::new_v4().as_bytes()[0]) % 101);
    let result = tokio::time::timeout_at(deadline, async {
        loop {
            let operation = store
                .get_operation(&accepted.app_id, &accepted.operation_id)
                .await?
                .ok_or_else(|| anyhow!("Accepted builder operation disappeared"))?;
            if operation.lifecycle_id != accepted.lifecycle_id {
                return Err(anyhow!("Builder lifecycle changed while waiting"));
            }
            match operation.state {
                UserAppOperationState::Succeeded => return Ok(operation),
                UserAppOperationState::Failed | UserAppOperationState::RecoveryRequired => {
                    return Err(anyhow!(
                        "Builder operation {} ended as {:?}: {}",
                        operation.operation_id,
                        operation.state,
                        operation
                            .error_message
                            .as_deref()
                            .unwrap_or("Operation result requires inspection")
                    ));
                }
                _ => {}
            }
            if let Some(receiver) = signal.as_mut() {
                tokio::select! {
                    result = receiver.changed() => { if result.is_err() { signal = None; } }
                    _ = tokio::time::sleep(delay + jitter) => {}
                }
            } else {
                tokio::time::sleep(delay + jitter).await;
            }
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    })
    .await;
    result.map_err(|_| shared_types::UserAppWaitTimeout {
        operation_id: Some(accepted.operation_id.clone()),
    })?
}

/// Resume only proven completion. No create, wake, or resource mutation occurs.
pub(super) async fn reconcile_completed(
    state: &AppState,
    snapshot: &UserAppOperationRecord,
) -> Result<bool> {
    if snapshot.kind != UserAppOperationKind::EnsureBuilder
        || !shared_types::userapp_operation_has_final_evidence(snapshot)
    {
        return Ok(false);
    }
    let Some(_local) = super::lifecycle::try_acquire(&snapshot.app_id).await else {
        return Ok(false);
    };
    let evidence: shared_types::BuilderCreationEvidence =
        serde_json::from_value(snapshot.checkpoint.clone())?;
    evidence
        .validate_operation(snapshot)
        .map_err(anyhow::Error::msg)?;
    let reserved = state
        .userapp_store
        .reserve_completed_operation(snapshot)
        .await?;
    let result = async {
        let context = &evidence.target.context;
        // evidence 持有的是创建时实例上下文（app_id 槽=复合 identifier）——
        // 按其恢复定位/注册，不从纯 app_id 重派生。
        let current =
            super::adoption::capture_bound_target(state, &evidence.target.context).await?;
        let info = super::cross_verify_registration(
            state,
            &snapshot.app_id,
            &context.app_id,
            &evidence.container,
        )
        .await?;
        let info = validate_completed_resource(&evidence, &current, info)?;
        super::register_builder(state, &context.app_id, &info)?;
        Ok::<_, anyhow::Error>(info)
    }
    .await;
    match result {
        Ok(info) => {
            progress(
                &state.userapp_store,
                &reserved,
                &evidence.target.context.executor_id,
                UserAppOperationState::Succeeded,
                "creation_result",
                serde_json::to_value(info)?,
                None,
            )
            .await?;
        }
        Err(error) => {
            progress(
                &state.userapp_store,
                &reserved,
                &evidence.target.context.executor_id,
                UserAppOperationState::RecoveryRequired,
                &reserved.step,
                reserved.checkpoint.clone(),
                Some(error.to_string()),
            )
            .await?;
            return Err(error);
        }
    }
    Ok(true)
}

/// Pure identity check shared by finalization and its deterministic regression.
/// It cannot start, wake, or otherwise mutate a runtime resource.
fn validate_completed_resource(
    evidence: &shared_types::BuilderCreationEvidence,
    current: &shared_types::BuilderControlTarget,
    ready: Option<ContainerBasicInfo>,
) -> Result<ContainerBasicInfo> {
    current.validate().map_err(anyhow::Error::msg)?;
    if current.context != evidence.target.context
        || current.workload.as_ref().map(|item| &item.uid)
            != evidence.target.workload.as_ref().map(|item| &item.uid)
        || current.pod.as_ref().map(|item| &item.uid)
            != evidence.target.pod.as_ref().map(|item| &item.uid)
    {
        return Err(anyhow!("Completed builder resource was replaced"));
    }
    let info = ready.ok_or_else(|| anyhow!("Completed builder is no longer ready"))?;
    if info.container_id != evidence.container.container_id {
        return Err(anyhow!("Completed builder resource was replaced"));
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn expired_creation_retains_observation_and_discards_late_success() {
        let local = super::super::lifecycle::acquire("budget-observation").await;
        let (complete, result) = tokio::sync::oneshot::channel();
        let (recorded, checkpoint) = tokio::sync::oneshot::channel();
        let (expire, expiry) = tokio::sync::oneshot::channel();
        let (started, start) = tokio::sync::oneshot::channel();
        let work = async move {
            let _local = local;
            observe_creation_budget(
                async {
                    expiry.await.expect("expiry");
                },
                async {
                    started.send(()).expect("start observer");
                    result.await.map_err(anyhow::Error::from)
                },
                async {
                    recorded.send(()).expect("observer");
                    Ok(())
                },
            )
            .await
        };
        let worker = tokio::spawn(work);
        tokio::time::timeout(Duration::from_secs(1), start)
            .await
            .expect("start deadline")
            .expect("started");
        expire.send(()).expect("expire active operation");
        tokio::time::timeout(Duration::from_secs(1), checkpoint)
            .await
            .expect("checkpoint deadline")
            .expect("recorded");
        assert!(!worker.is_finished());
        assert!(
            super::super::lifecycle::try_acquire("budget-observation")
                .await
                .is_none()
        );
        complete.send("late resource").expect("still observed");
        assert!(
            tokio::time::timeout(Duration::from_secs(1), worker)
                .await
                .expect("worker deadline")
                .expect("worker")
                .expect("observation")
                .is_none()
        );
        assert!(
            super::super::lifecycle::try_acquire("budget-observation")
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn expired_checkpoint_failure_still_observes_remote_completion() {
        let (complete, result) = tokio::sync::oneshot::channel::<()>();
        let (recorded, checkpoint) = tokio::sync::oneshot::channel();
        let (expire, expiry) = tokio::sync::oneshot::channel();
        let (started, start) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(observe_creation_budget(
            async {
                expiry.await.expect("expiry");
            },
            async {
                started.send(()).expect("start observer");
                result.await.map_err(anyhow::Error::from)
            },
            async {
                recorded.send(()).expect("observer");
                Err(anyhow!("database unavailable"))
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), start)
            .await
            .expect("start deadline")
            .expect("started");
        expire.send(()).expect("expire active operation");
        tokio::time::timeout(Duration::from_secs(1), checkpoint)
            .await
            .expect("checkpoint deadline")
            .expect("attempted");
        assert!(!worker.is_finished());
        complete.send(()).expect("still observed");
        assert!(
            tokio::time::timeout(Duration::from_secs(1), worker)
                .await
                .expect("worker deadline")
                .expect("worker")
                .is_err()
        );
    }

    #[tokio::test]
    async fn expired_budget_never_starts_unpolled_creation() {
        let result = observe_creation_budget::<()>(
            std::future::ready(()),
            async { panic!("expired creation must never start") },
            async { Ok(()) },
        )
        .await
        .expect("deadline checkpoint");
        assert!(result.is_none());
    }

    async fn admitted(
        store: &Arc<dyn UserAppLifecycleStore>,
        app_id: &str,
    ) -> UserAppOperationRecord {
        store
            .ensure_identity(app_id, "owner")
            .await
            .expect("identity");
        match store
            .admit(&UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: app_id.into(),
                user_id: "owner".into(),
                lifecycle_id: None,
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: None,
                request_fingerprint: "a".repeat(64),
                kind: UserAppOperationKind::EnsureBuilder,
            })
            .await
            .expect("admit")
        {
            UserAppAdmissionOutcome::Accepted(record) => record,
            _ => panic!("fresh application must admit once"),
        }
    }
    #[tokio::test]
    async fn final_creation_evidence_survives_terminal_failure_and_rejects_wrong_identity() {
        let directory = tempfile::tempdir().expect("directory");
        let store: Arc<dyn UserAppLifecycleStore> = Arc::new(
            rcoder_storage::userapp_lifecycle::SqliteUserAppStore::open(
                &directory.path().join("userapp.sqlite3"),
            )
            .await
            .expect("store"),
        );
        let admitted = admitted(&store, "completion").await;
        let running = progress(
            &store,
            &admitted,
            "worker",
            UserAppOperationState::Running,
            "claimed",
            serde_json::Value::Null,
            None,
        )
        .await
        .expect("claim");
        assert!(
            store.reserve_completed_operation(&running).await.is_err(),
            "unknown creation cannot recover"
        );
        let info = ContainerBasicInfo {
            container_id: "physical".into(),
            container_name: "builder".into(),
            container_ip: "127.0.0.1".into(),
            internal_port: 60000,
            external_port: 60000,
            project_id: "completion".into(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: "http://127.0.0.1:60000".into(),
        };
        let evidence = shared_types::BuilderCreationEvidence {
            creation_lease_released: true,
            target: shared_types::BuilderControlTarget {
                resource_binding: None,
                pod: None,
                workload: Some(shared_types::AppResourceIdentity {
                    kind: shared_types::AppResourceKind::Container,
                    name: "builder".into(),
                    uid: "physical".into(),
                    resource_version: None,
                }),
                context: shared_types::UserAppExecutionContext {
                    app_id: running.app_id.clone(),
                    user_id: "owner".into(),
                    lifecycle_id: running.lifecycle_id.clone(),
                    operation_id: running.operation_id.clone(),
                    executor_id: "worker".into(),
                    request_fingerprint: running.request_fingerprint.clone(),
                },
            },
            container: info,
        };
        assert!(
            validate_completed_resource(
                &evidence,
                &evidence.target,
                Some(evidence.container.clone())
            )
            .is_ok()
        );
        assert!(validate_completed_resource(&evidence, &evidence.target, None).is_err());
        let mut replacement = evidence.target.clone();
        replacement.workload.as_mut().expect("workload").uid = "replacement".into();
        assert!(
            validate_completed_resource(&evidence, &replacement, Some(evidence.container.clone()))
                .is_err()
        );
        let mut wrong_owner = evidence.target.clone();
        wrong_owner.context.user_id = "other-owner".into();
        assert!(
            validate_completed_resource(&evidence, &wrong_owner, Some(evidence.container.clone()))
                .is_err()
        );
        let mut other_pod = evidence.container.clone();
        other_pod.container_id = "other-pod".into();
        assert!(validate_completed_resource(&evidence, &evidence.target, Some(other_pod)).is_err());
        let mut wrong_owner_evidence = evidence.clone();
        wrong_owner_evidence.target.context.user_id = "foreign-owner".into();
        let invalid_owner = progress(
            &store,
            &running,
            "worker",
            UserAppOperationState::Running,
            "builder_ready_confirmed",
            serde_json::to_value(&wrong_owner_evidence).expect("encode"),
            None,
        )
        .await
        .expect("persist forged owner evidence for storage authorization test");
        assert!(
            store
                .reserve_completed_operation(&invalid_owner)
                .await
                .is_err(),
            "stored owner must authorize final recovery"
        );
        let saved = progress(
            &store,
            &invalid_owner,
            "worker",
            UserAppOperationState::Running,
            "builder_ready_confirmed",
            serde_json::to_value(&evidence).expect("encode"),
            None,
        )
        .await
        .expect("checkpoint");
        assert!(shared_types::userapp_operation_has_final_evidence(&saved));
        for field in ["operation", "lifecycle", "executor", "physical", "lease"] {
            let mut invalid = evidence.clone();
            match field {
                "operation" => invalid.target.context.operation_id = "other".into(),
                "lifecycle" => invalid.target.context.lifecycle_id = "other".into(),
                "executor" => invalid.target.context.executor_id = "other".into(),
                "physical" => invalid.container.container_id = "replacement".into(),
                _ => invalid.creation_lease_released = false,
            }
            let mut bad = saved.clone();
            bad.checkpoint = serde_json::to_value(invalid).expect("encode");
            assert!(
                !shared_types::userapp_operation_has_final_evidence(&bad),
                "{field}"
            );
        }
        assert!(
            progress(
                &store,
                &running,
                "worker",
                UserAppOperationState::Succeeded,
                "creation_result",
                serde_json::Value::Null,
                None
            )
            .await
            .is_err(),
            "stale terminal CAS must fail"
        );
        let interrupted = progress(
            &store,
            &saved,
            "worker",
            UserAppOperationState::RecoveryRequired,
            &saved.step,
            saved.checkpoint.clone(),
            Some("terminal write interrupted".into()),
        )
        .await
        .expect("interrupt");
        let reserved = store
            .reserve_completed_operation(&interrupted)
            .await
            .expect("reserve released creation without lease receipt");
        assert!(
            store
                .reserve_completed_operation(&interrupted)
                .await
                .is_err(),
            "only one finalizer reserves the snapshot"
        );
        progress(
            &store,
            &reserved,
            "worker",
            UserAppOperationState::Succeeded,
            "creation_result",
            serde_json::to_value(evidence.container).expect("encode"),
            None,
        )
        .await
        .expect("SQL-only finalization");
    }
    #[tokio::test]
    async fn late_subscriber_reads_committed_completion_even_after_signal_removal() {
        let directory = tempfile::tempdir().expect("directory");
        let store: Arc<dyn UserAppLifecycleStore> = Arc::new(
            rcoder_storage::userapp_lifecycle::SqliteUserAppStore::open(
                &directory.path().join("userapp.sqlite3"),
            )
            .await
            .expect("store"),
        );
        let accepted = admitted(&store, "late").await;
        let running = progress(
            &store,
            &accepted,
            "executor",
            UserAppOperationState::Running,
            "claimed",
            serde_json::Value::Null,
            None,
        )
        .await
        .expect("claim");
        progress(
            &store,
            &running,
            "executor",
            UserAppOperationState::Succeeded,
            "completed",
            serde_json::json!({"container_id":"physical-id"}),
            None,
        )
        .await
        .expect("complete");
        let result = wait_record(&store, &accepted, Instant::now() + Duration::from_secs(1))
            .await
            .expect("late subscriber");
        assert_eq!(result.checkpoint["container_id"], "physical-id");
    }
    #[tokio::test]
    async fn timeout_does_not_cancel_or_resubmit_accepted_operation() {
        let directory = tempfile::tempdir().expect("directory");
        let store: Arc<dyn UserAppLifecycleStore> = Arc::new(
            rcoder_storage::userapp_lifecycle::SqliteUserAppStore::open(
                &directory.path().join("userapp.sqlite3"),
            )
            .await
            .expect("store"),
        );
        let accepted = admitted(&store, "timeout").await;
        let running = progress(
            &store,
            &accepted,
            "executor",
            UserAppOperationState::Running,
            "claimed",
            serde_json::Value::Null,
            None,
        )
        .await
        .expect("claim");
        let error = wait_record(&store, &accepted, Instant::now())
            .await
            .expect_err("deadline");
        assert_eq!(
            error
                .downcast_ref::<shared_types::UserAppWaitTimeout>()
                .expect("typed wait timeout")
                .operation_id
                .as_deref(),
            Some(accepted.operation_id.as_str())
        );
        let unchanged = store
            .get_operation("timeout", &accepted.operation_id)
            .await
            .expect("read")
            .expect("operation");
        assert_eq!(unchanged.revision, running.revision);
        assert_eq!(unchanged.state, UserAppOperationState::Running);
        progress(
            &store,
            &running,
            "executor",
            UserAppOperationState::Succeeded,
            "completed",
            serde_json::Value::Null,
            None,
        )
        .await
        .expect("worker still completes");
        wait_record(&store, &accepted, Instant::now() + Duration::from_secs(1))
            .await
            .expect("subsequent waiter sees result");
    }
}
