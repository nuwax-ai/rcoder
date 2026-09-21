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

fn spawn_operation(
    state: &AppState,
    record: &UserAppOperationRecord,
    instance: &str,
    lease: OwnedMutexGuard<()>,
    flight: shared_types::FlightGuard,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let (signal, _) = watch::channel(0);
    SIGNALS
        .lock()
        .map_err(|_| anyhow!("Builder notification registry lock poisoned"))?
        .insert(record.operation_id.clone(), signal.clone());
    let worker_state = state.clone();
    let worker_record = record.clone();
    let instance = instance.to_string();
    let worker = tokio::spawn(async move {
        // R02：在途协调任务门闸——关机时等待本任务收束后再关闭存储
        // Both the observer and the actual execution retain admission. A panic
        // in the observer must not make a detached execution invisible to drain.
        let flight = Arc::new(flight);
        let execution_flight = flight.clone();
        let _flight = flight;
        let executor = uuid::Uuid::new_v4().to_string();
        let owned = worker_state.clone();
        let claimed_id = executor.clone();
        let initial = worker_record.clone();
        let execution_signal = signal.clone();
        let execution = tokio::spawn(async move {
            let _flight = execution_flight;
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
                shared_types::UserAppExecutionContext {
                    app_id: instance.clone(),
                    lifecycle_id: claimed.lifecycle_id.clone(),
                    operation_id: claimed.operation_id.clone(),
                    executor_id: claimed_id.clone(),
                    request_fingerprint: claimed.request_fingerprint.clone(),
                },
            );
            let creation = async {
                let result = creation.await;
                if Instant::now() >= ready_deadline {
                    // Only readonly observation after expiry; never register
                    // the instance or replay creation from this old executor.
                    match tokio::time::timeout(Duration::from_secs(30),
                        record_late_creation_result(&owned, &claimed, &result)).await
                    {
                        Ok(Ok(())) => {},
                        Ok(Err(error)) => tracing::warn!(%error,
                            operation_id = %claimed.operation_id, "Late builder evidence could not be saved"),
                        Err(error) => tracing::warn!(%error,
                            operation_id = %claimed.operation_id, "Late builder evidence observation timed out"),
                    }
                }
                result
            };
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
                    super::confirm_builder_ready_inner(
                        &owned,
                        &claimed.app_id,
                        &instance,
                        info,
                        Some(&claimed),
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
                        // capture 侧按 app identifier 定位容器（与上方
                        // create_builder_inner 同源；应用共享后两者恒等）
                        app_id: instance.clone(),
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
                    (
                        UserAppOperationState::Succeeded,
                        serde_json::to_value(info)?,
                        None,
                    )
                },
                Err(error) => {
                    let cancelled = matches!(error.downcast_ref::<container_runtime_api::ContainerRuntimeError>(),
                        Some(container_runtime_api::ContainerRuntimeError::CreationCancelled));
                    let rejected = error
                        .downcast_ref::<container_runtime_api::ContainerRuntimeError>()
                        .is_some_and(|e| {
                            matches!(
                                e,
                                container_runtime_api::ContainerRuntimeError::RequestRejected(_)
                            )
                        });
                    (
                        if rejected || cancelled {
                            UserAppOperationState::Failed
                        } else {
                            UserAppOperationState::RecoveryRequired
                        },
                        if cancelled { serde_json::json!({"creation_cancelled": true}) } else { serde_json::Value::Null },
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

async fn record_late_creation_result(
    state: &AppState,
    claimed: &UserAppOperationRecord,
    result: &Result<ContainerBasicInfo>,
) -> Result<()> {
    match result {
        Ok(info) => record_late_creation(state, claimed, info).await,
        Err(error) => {
            if matches!(
                error.downcast_ref::<container_runtime_api::ContainerRuntimeError>(),
                Some(container_runtime_api::ContainerRuntimeError::CreationCancelled)
            ) {
                let snapshot = state
                    .userapp_store
                    .get_operation(&claimed.app_id, &claimed.operation_id)
                    .await?
                    .ok_or_else(|| anyhow!("Cancelled builder operation disappeared"))?;
                if snapshot.lifecycle_id != claimed.lifecycle_id
                    || snapshot.executor_id != claimed.executor_id
                    || snapshot.request_fingerprint != claimed.request_fingerprint
                {
                    return Err(anyhow!("Builder cancellation belongs to a stale executor"));
                }
                state
                    .userapp_store
                    .finalize_builder_creation_cancellation(&snapshot)
                    .await?;
                return Ok(());
            }
            let Some(container_runtime_api::ContainerRuntimeError::RequestRejected(rejection)) =
                error.downcast_ref::<container_runtime_api::ContainerRuntimeError>()
            else {
                // A timeout/transport/cleanup failure remains uncertain.
                tracing::warn!(operation_id = %claimed.operation_id, error = %error,
                    "Late builder failure still requires runtime reconciliation");
                return Ok(());
            };
            let snapshot = state
                .userapp_store
                .get_operation(&claimed.app_id, &claimed.operation_id)
                .await?
                .ok_or_else(|| anyhow!("Late rejected builder operation disappeared"))?;
            if snapshot.lifecycle_id != claimed.lifecycle_id
                || snapshot.executor_id != claimed.executor_id
                || snapshot.request_fingerprint != claimed.request_fingerprint
            {
                return Err(anyhow!(
                    "Late builder rejection belongs to a stale executor"
                ));
            }
            state
                .userapp_store
                .finalize_builder_creation_rejection(&snapshot, rejection)
                .await?;
            Ok(())
        }
    }
}

/// Persist a late create response after physical identity verification, before
/// waiting for management readiness. The scanner completes that observation.
async fn record_late_creation(
    state: &AppState,
    claimed: &UserAppOperationRecord,
    info: &ContainerBasicInfo,
) -> Result<()> {
    let snapshot = state
        .userapp_store
        .get_operation(&claimed.app_id, &claimed.operation_id)
        .await?
        .ok_or_else(|| anyhow!("Late builder operation disappeared"))?;
    if snapshot.state != UserAppOperationState::RecoveryRequired
        || snapshot.step != "creation_confirmation_timed_out"
        || snapshot.executor_id != claimed.executor_id
        || snapshot.lifecycle_id != claimed.lifecycle_id
        || snapshot.request_fingerprint != claimed.request_fingerprint
    {
        return Ok(());
    }
    let context = shared_types::UserAppExecutionContext {
        app_id: claimed.app_id.clone(),
        lifecycle_id: claimed.lifecycle_id.clone(),
        operation_id: claimed.operation_id.clone(),
        executor_id: claimed
            .executor_id
            .clone()
            .ok_or_else(|| anyhow!("Late builder executor missing"))?,
        request_fingerprint: claimed.request_fingerprint.clone(),
    };
    let target = super::adoption::capture_bound_target(state, &context).await?;
    let evidence = shared_types::BuilderCreationEvidence {
        creation_lease_released: true,
        target,
        container: info.clone(),
    };
    evidence
        .validate_operation(&snapshot)
        .map_err(anyhow::Error::msg)?;
    state
        .userapp_store
        .confirm_builder_creation_recovery(&snapshot, &evidence, false)
        .await?;
    Ok(())
}

/// Bridge runtime completion to the database after the worker or connection
/// disappeared. The runtime releases only an acknowledged original lease.
pub(super) async fn reconcile_runtime_receipt(
    state: &AppState,
    snapshot: &UserAppOperationRecord,
) -> Result<bool> {
    if !shared_types::userapp_builder_creation_needs_runtime_receipt(snapshot) {
        return Ok(false);
    }
    let Some(_local) = super::lifecycle::try_acquire(&snapshot.app_id).await else {
        return Ok(false);
    };
    if state
        .userapp_store
        .get_operation(&snapshot.app_id, &snapshot.operation_id)
        .await?
        .as_ref()
        != Some(snapshot)
    {
        return Ok(false);
    }
    let context = shared_types::UserAppExecutionContext {
        app_id: snapshot.app_id.clone(),
        lifecycle_id: snapshot.lifecycle_id.clone(),
        operation_id: snapshot.operation_id.clone(),
        executor_id: snapshot
            .executor_id
            .clone()
            .ok_or_else(|| anyhow!("Builder recovery executor missing"))?,
        request_fingerprint: snapshot.request_fingerprint.clone(),
    };
    let recovered = tokio::time::timeout(
        Duration::from_secs(30),
        state.runtime().recover_builder_creation(&context),
    )
    .await
    .context("Builder runtime receipt recovery deadline exceeded")?;
    let evidence = match recovered {
        Ok(Some(evidence)) => evidence,
        Ok(None) => return Ok(false),
        Err(container_runtime_api::ContainerRuntimeError::CreationCancelled) => {
            state
                .userapp_store
                .finalize_builder_creation_cancellation(snapshot)
                .await?;
            return Ok(true);
        }
        Err(error) => return Err(error.into()),
    };
    evidence
        .validate_operation(snapshot)
        .map_err(anyhow::Error::msg)?;
    state
        .userapp_store
        .confirm_builder_creation_recovery(snapshot, &evidence, false)
        .await?;
    Ok(true)
}

/// A persisted create response is not a readiness result. Observe once per
/// recovery scan; never reissue creation or publish a project registration.
pub(super) async fn reconcile_created(
    state: &AppState,
    snapshot: &UserAppOperationRecord,
) -> Result<bool> {
    if snapshot.kind != UserAppOperationKind::EnsureBuilder
        || snapshot.state != UserAppOperationState::RecoveryRequired
        || snapshot.step != "builder_created_observed"
    {
        return Ok(false);
    }
    let Some(_local) = super::lifecycle::try_acquire(&snapshot.app_id).await else {
        return Ok(false);
    };
    let mut evidence: shared_types::BuilderCreationEvidence =
        serde_json::from_value(snapshot.checkpoint.clone())?;
    evidence
        .validate_operation(snapshot)
        .map_err(anyhow::Error::msg)?;
    let Some(info) = tokio::time::timeout(
        Duration::from_secs(30),
        observe_created_ready(state, &evidence),
    )
    .await
    .context("Builder recovery observation deadline exceeded")??
    else {
        return Ok(false);
    };
    evidence.container = info;
    state
        .userapp_store
        .confirm_builder_creation_recovery(snapshot, &evidence, true)
        .await?;
    Ok(true)
}

/// 围栏证据：物理状态只读观察结论。
enum FenceEvidence {
    /// 状态确定且可观察（附留痕）——该生命周期已有一次完成的变更落地，
    /// 后继操作可安全重走完整 admission + capture + precondition 流程。
    Observed(serde_json::Value),
    /// 观察失败/身份不符/kind 无谓词：保持围栏（保守面不变）。
    Insufficient,
}

/// per-kind 证据谓词。判定规则统一为：**物理状态确定（无死执行者在途写）**，
/// 而非"操作目标已达成"——围栏的职责是防半应用状态上的并发变更，而围栏
/// 落盘时执行者已携带已知错误返回（`OwnedOperation::fail`），不存在在途
/// 写；后继操作自带 admission/capture/precondition 防线。
async fn observe_fence_evidence(
    state: &AppState,
    record: &UserAppOperationRecord,
) -> FenceEvidence {
    match record.kind {
        // dev 族：同 app+lifecycle 的存活 builder（指纹无关观察路径——
        // 被本生命周期另一次操作 ensure 出来的 builder 同样算数，正是
        // "被取代"情形）。
        UserAppOperationKind::EnsureBuilder | UserAppOperationKind::AdoptBuilder => {
            let context = shared_types::UserAppExecutionContext {
                app_id: record.app_id.clone(),
                lifecycle_id: record.lifecycle_id.clone(),
                operation_id: record.operation_id.clone(),
                executor_id: record
                    .executor_id
                    .clone()
                    .unwrap_or_else(|| "fenced-settler".into()),
                request_fingerprint: record.request_fingerprint.clone(),
            };
            match super::adoption::capture_bound_target(state, &context).await {
                // 活体工作负载是唯一可接受证据：identity 不符（Err）或
                // workload 缺失（Ok(None)）都保持围栏。
                Ok(target) if target.workload.is_some() => {
                    FenceEvidence::Observed(serde_json::json!({
                        "kind": "live_builder",
                        "workload": target.workload,
                        "observed_at_us": chrono::Utc::now().timestamp_micros(),
                    }))
                }
                _ => FenceEvidence::Insufficient,
            }
        }
        // prod 发布族：活代 generation + 运行相位。generation == 本操作 =
        // 本次 rollout 已落地（app 154 事故形态：patch 后零 pod 窗口捕获
        // 失败，rollout 事后照常完成）；generation 属后来操作 = 被取代
        // （与 builder 族"被另一次操作 ensure"同义）。
        UserAppOperationKind::StartDeployment | UserAppOperationKind::RestartDeployment => {
            let Ok(spec) = state.runtime().get_app_container_spec(&record.app_id).await else {
                return FenceEvidence::Insufficient;
            };
            let generation = spec
                .env
                .as_ref()
                .and_then(|env| env.get(shared_types::APP_DEPLOY_GENERATION_ID).cloned());
            let Some(generation) = generation else {
                return FenceEvidence::Insufficient;
            };
            let superseded = generation != record.operation_id;
            match state.app_service.get_app(&record.app_id).await {
                Ok(info) if info.phase == "Running" => FenceEvidence::Observed(serde_json::json!({
                    "kind": if superseded { "superseded_by_newer_generation" }
                              else { "rollout_completed" },
                    "live_generation": generation,
                    "phase": info.phase,
                    "ready_replicas": info.ready_replicas,
                    "observed_at_us": chrono::Utc::now().timestamp_micros(),
                })),
                // 查询失败或相位未定：保持围栏。
                _ => FenceEvidence::Insufficient,
            }
        }
        // prod 启停族：相位确定即收束（记录实际相位留痕——目标未达成时
        // 后继显式操作会重新观测并执行，好于永久挡死）。
        UserAppOperationKind::Start | UserAppOperationKind::Restart => {
            match state.app_service.get_app(&record.app_id).await {
                Ok(info) => FenceEvidence::Observed(serde_json::json!({
                    "kind": "prod_phase_definite",
                    "phase": info.phase,
                    "replicas": info.replicas,
                    "ready_replicas": info.ready_replicas,
                    "observed_at_us": chrono::Utc::now().timestamp_micros(),
                })),
                Err(_) => FenceEvidence::Insufficient,
            }
        }
        UserAppOperationKind::Stop => match state.app_service.get_app(&record.app_id).await {
            Ok(info) if info.replicas == 0 => FenceEvidence::Observed(serde_json::json!({
                "kind": "prod_stopped",
                "phase": info.phase,
                "observed_at_us": chrono::Utc::now().timestamp_micros(),
            })),
            Ok(info) => FenceEvidence::Observed(serde_json::json!({
                "kind": "prod_still_running",
                "phase": info.phase,
                "replicas": info.replicas,
                "observed_at_us": chrono::Utc::now().timestamp_micros(),
            })),
            Err(_) => FenceEvidence::Insufficient,
        },
        _ => FenceEvidence::Insufficient,
    }
}

/// Auto-settle a fenced operation whose physical state is verifiably definite:
/// the interrupted operation is settled as Failed with observation evidence —
/// never Succeeded, because this operation itself did not complete — which
/// frees the admission slot so the next explicit operation proceeds normally.
/// Covers every kind with an evidence predicate (test-env app 151/155: fenced
/// EnsureBuilder blocked every chat with "Container operation failed"; app
/// 154: fenced start_deployment blocked redeploy with ERR_CONFLICT until an
/// operator ran manual SQL). Identity mismatch or absent resources keep the
/// fence protected.
pub(super) async fn reconcile_fenced_ensure(
    state: &AppState,
    snapshot: &UserAppOperationRecord,
) -> Result<()> {
    if snapshot.state != UserAppOperationState::RecoveryRequired {
        return Ok(());
    }
    let Some(_local) = super::lifecycle::try_acquire(&snapshot.app_id).await else {
        tracing::debug!(
            operation_id = %snapshot.operation_id,
            app_id = %snapshot.app_id,
            "Fence settle skipped: app lifecycle lock is contended this tick"
        );
        return Ok(());
    };
    // Re-read under the lock: another executor may have settled or advanced
    // the operation since the scan snapshot was taken.
    let current = state
        .userapp_store
        .get_operation(&snapshot.app_id, &snapshot.operation_id)
        .await?;
    let Some(record) = current.filter(|record| {
        record.state == UserAppOperationState::RecoveryRequired && record.kind == snapshot.kind
    }) else {
        tracing::debug!(
            operation_id = %snapshot.operation_id,
            app_id = %snapshot.app_id,
            "Fence settle skipped: record advanced or settled concurrently"
        );
        return Ok(());
    };
    // 无执行者声明的记录不可被扫描器收束（store 侧同样拒绝）。
    if record.executor_id.is_none() {
        tracing::warn!(
            operation_id = %record.operation_id,
            app_id = %record.app_id,
            "Fence kept: record has no executor claim"
        );
        return Ok(());
    }
    let evidence = match observe_fence_evidence(state, &record).await {
        FenceEvidence::Observed(evidence) => evidence,
        FenceEvidence::Insufficient => {
            // 线上定位锚点：此日志出现说明分发与执行都正常、卡在证据谓词
            // （观察失败或身份不符）。每 app 每扫描周期一条，节流由扫描
            // 器节奏（5s）天然限定。
            tracing::warn!(
                operation_id = %record.operation_id,
                app_id = %record.app_id,
                kind = ?record.kind,
                step = %record.step,
                "Fence kept: evidence predicate returned insufficient"
            );
            return Ok(());
        }
    };
    // 经 store 的 sanctioned 终态化路径（内部 Running 跳转满足状态机独占
    // 门，纯记账不授权运行时工作）——直接 advance(Failed) 会被
    // domain::advance 的 RecoveryRequired 转移门拒绝（f49b594d 潜伏 bug：
    // settler 从未真正收束过任何围栏）。
    state
        .userapp_store
        .settle_fenced_operation(&record, &evidence)
        .await?;
    tracing::warn!(
        operation_id = %record.operation_id,
        app_id = %record.app_id,
        kind = ?record.kind,
        "Fenced operation settled as Failed: physical state verified definite"
    );
    Ok(())
}

async fn observe_created_ready(
    state: &AppState,
    evidence: &shared_types::BuilderCreationEvidence,
) -> Result<Option<ContainerBasicInfo>> {
    let context = &evidence.target.context;
    let before = super::adoption::capture_bound_target(state, context).await?;
    validate_completed_resource(evidence, &before, Some(evidence.container.clone()))?;
    let Some(runtime) = state
        .runtime()
        .find_container(&context.app_id, &shared_types::ServiceType::UserappBuilder)
        .await?
    else {
        return Ok(None);
    };
    super::validate_builder_identity(&context.app_id, &runtime)?;
    if runtime.container_id != evidence.container.container_id {
        return Err(anyhow!("Builder physical identity changed during recovery"));
    }
    if runtime.status != container_runtime_api::ContainerRuntimeStatus::Running {
        return Ok(None);
    }
    let info = super::refreshed_registration(&evidence.container, &runtime)
        .unwrap_or_else(|| evidence.container.clone());
    if !super::probe_file_server(&super::dev_file_server_addr(state, &info)).await {
        return Ok(None);
    }
    let after = super::adoption::capture_bound_target(state, context).await?;
    Ok(Some(validate_completed_resource(
        evidence,
        &after,
        Some(info),
    )?))
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
    // 恢复指纹按共享模型重算（应用共享：instance == app_id）
    let instance = current.app_id.clone();
    if instance_fingerprint(state, &current.app_id)? != current.request_fingerprint {
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
    spawn_operation(
        state,
        &current,
        &instance,
        lease,
        state.userapp_op_flight.guard()?,
    )?
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
    let verified = super::verify_registration(state, &accepted.app_id, instance, &info, false)
        .await?
        .ok_or_else(|| anyhow!("Completed builder is no longer running"))?;
    if verified.container_id != info.container_id {
        return Err(anyhow!("Completed builder resource was replaced"));
    }
    // A successful historical result does not authorize publishing an address
    // after a later Stop/Restart has taken control. Cache readers still recheck
    // runtime identity; this registration is never a new execution authority.
    state
        .userapp_store
        .check_compute_access(
            &accepted.app_id,
            shared_types::UserAppOperationScope::Dev,
            false,
        )
        .await?;
    super::register_builder(state, instance, &verified)?;
    Ok(verified)
}

/// RecoveryRequired 骑代宽限。该状态是"需要检查"的**中间态**而非终态：
/// 预算观察器（creation_confirmation_timed_out）与 worker 中断观察都会
/// 写入它并明确注释"remote execution is still being observed"，恢复扫描器
/// 5s 一轮核验改判——证据满足→Succeeded，真实围栏→settler 收 Failed。
/// 等待方在此态上立即宣判过会把"再等几秒就好"变成用户可见错误（测试
/// 环境 app 159 首开竞态：创建中的瞬时检查态被两个并发 ensure 判死→
/// Java 5000，5 秒后创建实际成功）。持续超过宽限仍不改判才按终态处理：
/// 真实围栏的最坏暴露从 0s 变为 ≤宽限+一个轮询周期。
const RECOVERY_REQUIRED_GRACE: Duration = Duration::from_secs(15);

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
    // 当前骑代窗口的起点：状态离开 RecoveryRequired 即重置，每个检查窗口
    // 都获得完整宽限。
    let mut fenced_since: Option<Instant> = None;
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
                UserAppOperationState::Failed => {
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
                UserAppOperationState::RecoveryRequired => {
                    let since = *fenced_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= RECOVERY_REQUIRED_GRACE {
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
                }
                _ => {
                    fenced_since = None;
                }
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
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        observe_created_ready(state, &evidence),
    )
    .await
    .context("Builder completion observation deadline exceeded")
    .and_then(|result| result)
    .and_then(|info| info.ok_or_else(|| anyhow!("Builder management endpoint is not ready")));
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
        store.ensure_identity(app_id).await.expect("identity");
        match store
            .admit(&UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: app_id.into(),
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
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("userapp.turso.db"),
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
            workload_uid: None,
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
        let mut other_pod = evidence.container.clone();
        other_pod.container_id = "other-pod".into();
        assert!(validate_completed_resource(&evidence, &evidence.target, Some(other_pod)).is_err());
        let saved = progress(
            &store,
            &running,
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
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("userapp.turso.db"),
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
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("userapp.turso.db"),
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

// ============================================================================
// P1 围栏证据化收束测试（泛化 per-kind 谓词）
// ============================================================================

#[cfg(test)]
pub(crate) mod fence_settler_tests {
    use super::*;
    use crate::app_state::AppState;
    use arc_swap::ArcSwap;
    use container_runtime_api::{
        AgentContainerRuntime, ContainerRuntimeError, ContainerRuntimeResult, DeploymentStatus,
        UserAppDeploymentRuntime, WorkspaceRuntime,
    };
    use dashmap::DashMap;
    use shared_types::{
        AppResourceIdentity, AppResourceKind, BuilderControlTarget, ServiceType, UserAppAdmission,
        UserAppAdmissionOutcome, UserAppControlCommand, UserAppOperationKind,
    };
    use std::collections::HashMap;

    /// 可控运行态：deployment 相位/副本 + 活代 generation + builder 观察。
    #[derive(Default)]
    pub(crate) struct FenceRuntime {
        status: Mutex<Option<Option<DeploymentStatus>>>,
        generation: Mutex<Option<String>>,
        pub(crate) builder_workload: Mutex<Option<String>>,
    }

    impl FenceRuntime {
        pub(crate) fn scenario(
            status: Option<DeploymentStatus>,
            generation: Option<&str>,
        ) -> Arc<Self> {
            Arc::new(Self {
                status: Mutex::new(Some(status)),
                generation: Mutex::new(generation.map(str::to_string)),
                builder_workload: Mutex::new(None),
            })
        }
    }

    fn status_of(phase: &str, replicas: i32) -> DeploymentStatus {
        DeploymentStatus {
            app_id: "fenced".into(),
            replicas,
            ready_replicas: replicas,
            phase: phase.into(),
            message: None,
            pod_ip: None,
            node: None,
            restart_count: 0,
            started_at: None,
            ports: vec![],
            resource_version: Some("9".into()),
            recycle_enabled: None,
            idle_timeout_seconds: None,
            wake_on_traffic: None,
            created_at: None,
            deployment_uid: None,
        }
    }

    #[async_trait::async_trait]
    impl AgentContainerRuntime for FenceRuntime {
        async fn create_container(
            &self,
            _params: container_runtime_api::ContainerCreateParams,
        ) -> ContainerRuntimeResult<ContainerBasicInfo> {
            Err(ContainerRuntimeError::ContainerNotFound("fence".into()))
        }
        async fn get_container_info(
            &self,
            _project_id: &str,
        ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
            Ok(None)
        }
        async fn find_container(
            &self,
            _identifier: &str,
            _service_type: &ServiceType,
        ) -> ContainerRuntimeResult<Option<container_runtime_api::RuntimeContainerInfo>> {
            Ok(None)
        }
        async fn stop_container(&self, _project_id: &str) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn is_container_running(&self, _project_id: &str) -> ContainerRuntimeResult<bool> {
            Ok(false)
        }
        async fn list_containers(
            &self,
        ) -> ContainerRuntimeResult<Vec<container_runtime_api::RuntimeContainerInfo>> {
            Ok(vec![])
        }
        async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn health_check(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn inspect_builder_candidate(
            &self,
            _context: &shared_types::UserAppExecutionContext,
        ) -> ContainerRuntimeResult<BuilderControlTarget> {
            // workload=None → capture_bound_target 回落到 capture_builder_control。
            Ok(BuilderControlTarget {
                resource_binding: None,
                context: _context.clone(),
                workload: None,
                pod: None,
            })
        }
        async fn capture_builder_control(
            &self,
            context: &shared_types::UserAppExecutionContext,
        ) -> ContainerRuntimeResult<BuilderControlTarget> {
            let workload = self.builder_workload.lock().expect("lock").clone();
            Ok(BuilderControlTarget {
                resource_binding: None,
                context: context.clone(),
                workload: workload.map(|name| AppResourceIdentity {
                    kind: AppResourceKind::StatefulSet,
                    name,
                    uid: "builder-uid".into(),
                    resource_version: Some("3".into()),
                }),
                pod: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl WorkspaceRuntime for FenceRuntime {}

    #[async_trait::async_trait]
    impl UserAppDeploymentRuntime for FenceRuntime {
        async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
            Ok(vec![])
        }
        async fn get_deployment_status(
            &self,
            _app_id: &str,
        ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
            Ok(self.status.lock().expect("lock").clone().flatten())
        }
        async fn get_app_container_spec(
            &self,
            _app_id: &str,
        ) -> ContainerRuntimeResult<container_runtime_api::ContainerSpecSnapshot> {
            Ok(container_runtime_api::ContainerSpecSnapshot {
                command: None,
                env: Some(HashMap::from([(
                    shared_types::APP_DEPLOY_GENERATION_ID.to_string(),
                    self.generation
                        .lock()
                        .expect("lock")
                        .clone()
                        .unwrap_or_else(|| "absent".into()),
                )])),
                secrets: None,
                resources: None,
                health_check: None,
                ports: None,
            })
        }
    }

    pub(crate) async fn fence_state(
        runtime: Arc<FenceRuntime>,
        kind: UserAppOperationKind,
        operation_id: &str,
    ) -> (Arc<AppState>, tempfile::TempDir) {
        fence_state_with_terminal(
            runtime,
            kind,
            operation_id,
            UserAppOperationState::RecoveryRequired,
        )
        .await
    }

    async fn fence_state_with_terminal(
        runtime: Arc<FenceRuntime>,
        kind: UserAppOperationKind,
        operation_id: &str,
        terminal: UserAppOperationState,
    ) -> (Arc<AppState>, tempfile::TempDir) {
        let metadata_dir = tempfile::tempdir().expect("metadata directory");
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &metadata_dir.path().join("userapp.turso.db"),
            )
            .await
            .expect("store"),
        );
        let (state, _keep) =
            fence_state_on_store(runtime, kind, operation_id, terminal, store).await;
        // TempDir 必须活过测试：把 _keep 换成泄露（测试进程级可接受）。
        std::mem::forget(metadata_dir);
        (state, tempfile::tempdir().expect("placeholder dir"))
    }

    /// PG 后端变体：store 走 Postgres（集群形态），RCODER_PG_TEST_DSN 门控。
    #[cfg(feature = "rcoder-pg")]
    pub(crate) async fn fence_state_pg(
        runtime: Arc<FenceRuntime>,
        kind: UserAppOperationKind,
        operation_id: &str,
        dsn: &str,
    ) -> Arc<AppState> {
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::connect(
                &rcoder_storage::config::PostgresConfig {
                    url: Some(dsn.into()),
                    ..Default::default()
                },
            )
            .await
            .expect("PG store"),
        );
        let (state, _keep) = fence_state_on_store(
            runtime,
            kind,
            operation_id,
            UserAppOperationState::RecoveryRequired,
            store,
        )
        .await;
        std::mem::forget(_keep);
        state
    }

    async fn fence_state_on_store(
        runtime: Arc<FenceRuntime>,
        kind: UserAppOperationKind,
        operation_id: &str,
        terminal: UserAppOperationState,
        store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    ) -> (
        Arc<AppState>,
        Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    ) {
        let (adapter, _cleanup_rx) =
            crate::storage::ProjectAdapter::new("test-ns".to_string(), "cluster.local".to_string());
        let activity = Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(
            300,
        )));
        let manager_config = app_manager::config::AppManagerConfig {
            access_mode: app_manager::config::AppAccessMode::Docker,
            ..app_manager::config::AppManagerConfig::default()
        };
        let app_service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(
            app_manager::service::AppService::new(
                manager_config,
                runtime.clone(),
                activity.clone(),
                None,
                store.clone(),
            )
            .await
            .expect("AppService"),
        );
        let download_dir = tempfile::tempdir().expect("download directory");
        let (pod_created_tx, _) = tokio::sync::broadcast::channel(8);
        let state = Arc::new(AppState {
            userapp_store: store.clone(),
            userapp_store_control: store.clone(),
            userapp_op_flight: Arc::new(
                crate::userapp_builder::shutdown_gate::OperationFlightGate::default(),
            ),
            userapp_recovery_handle: Arc::new(Mutex::new(None)),
            config: crate::config::AppConfig::default(),
            projects: Arc::new(crate::storage::ProjectStoreBackend::Memory(Arc::new(
                adapter,
            ))),
            pingora_service: None,
            grpc_pool: Arc::new(crate::grpc::GrpcChannelPool::new()),
            session_stream_registry: Arc::new(crate::grpc::SessionStreamRegistry::new()),
            api_key_config: Arc::new(ArcSwap::from_pointee(
                crate::config::ApiKeyAuthConfig::default(),
            )),
            pod_creating: Arc::new(DashMap::new()),
            pod_created_tx: Arc::new(pod_created_tx),
            container_prefix_rcoder: "dev-rcoder".to_string(),
            container_prefix_computer: "computer-agent-runner".to_string(),
            runtime,
            cleanup_rx: Arc::new(Mutex::new(None)),
            agent_download_manager: Arc::new(
                agent_provisioning::AgentDownloadManager::new(download_dir.path())
                    .expect("download manager"),
            ),
            app_service,
            activity,
            cluster_domain: "cluster.local".to_string(),
        });
        // 构造围栏记录：admit(Pending) → advance(Running, executor) →
        // advance(RecoveryRequired, step=runtime_updated)。
        let lifecycle = store.ensure_identity("fenced").await.expect("identity");
        // EnsureBuilder/AdoptBuilder 无对应命令变体（命令↔kind 严格对应），
        // 传 None；其余 kind 按命令表构造，Deploy/Update 族携带执行输入摘要。
        let builder_family = matches!(
            kind,
            UserAppOperationKind::EnsureBuilder | UserAppOperationKind::AdoptBuilder
        );
        let input = shared_types::UserAppExecutionInput::new("{}".into());
        let command = if builder_family {
            None
        } else {
            Some(match kind {
                UserAppOperationKind::StartDeployment => UserAppControlCommand::Deploy {
                    restart: false,
                    input_digest: input.digest(),
                },
                UserAppOperationKind::RestartDeployment => UserAppControlCommand::Deploy {
                    restart: true,
                    input_digest: input.digest(),
                },
                UserAppOperationKind::Update => UserAppControlCommand::Update {
                    input_digest: input.digest(),
                },
                UserAppOperationKind::Restart => UserAppControlCommand::Restart,
                UserAppOperationKind::Stop => UserAppControlCommand::Stop {
                    wake_on_traffic: true,
                },
                _ => UserAppControlCommand::Start { traffic: false },
            })
        };
        let admitted = store
            .admit_with_input(
                &UserAppAdmission {
                    app_id: "fenced".into(),
                    lifecycle_id: Some(lifecycle.lifecycle_id.clone()),
                    operation_id: operation_id.into(),
                    request_id: Some(format!("req-{operation_id}")),
                    request_fingerprint: "cd".repeat(32),
                    kind,
                    command,
                    metadata: None,
                    runtime_policy_on_success: None,
                },
                (!builder_family).then_some(&input),
            )
            .await
            .expect("admit");
        let record = match admitted {
            UserAppAdmissionOutcome::Accepted(record) => record,
            other => panic!("unexpected admission: {other:?}"),
        };
        store
            .advance(&UserAppOperationProgress {
                app_id: record.app_id.clone(),
                lifecycle_id: record.lifecycle_id.clone(),
                operation_id: record.operation_id.clone(),
                expected_revision: record.revision,
                executor_id: "executor-fenced".into(),
                state: UserAppOperationState::Running,
                step: "claimed".into(),
                checkpoint: serde_json::Value::Null,
                error_code: None,
                error_message: None,
            })
            .await
            .expect("claim");
        let running = store
            .get_operation("fenced", operation_id)
            .await
            .expect("read")
            .expect("running record");
        store
            .advance(&UserAppOperationProgress {
                app_id: running.app_id.clone(),
                lifecycle_id: running.lifecycle_id.clone(),
                operation_id: running.operation_id.clone(),
                expected_revision: running.revision,
                executor_id: "executor-fenced".into(),
                state: terminal,
                step: "runtime_updated".into(),
                checkpoint: serde_json::json!({"resource": {"container_id": "c1"}}),
                error_code: Some("ERR_BACKEND_ERROR".into()),
                error_message: Some(
                    "Capture explicit database target: Owned management container is not running"
                        .into(),
                ),
            })
            .await
            .expect("fence");
        (state, store)
    }

    pub(crate) async fn settled(
        state: &Arc<AppState>,
        operation_id: &str,
    ) -> UserAppOperationRecord {
        state
            .userapp_store
            .get_operation("fenced", operation_id)
            .await
            .expect("read")
            .expect("record")
    }

    /// app 154 事故类：发布 rollout 已完成（活代=本操作、相位 Running）→
    /// 围栏必须被证据化收束为 Failed（修复前永久卡死，需人工 SQL）。
    #[tokio::test]
    async fn start_deployment_fence_settles_when_rollout_completed() {
        let operation_id = "op-rollout-done";
        let runtime = FenceRuntime::scenario(Some(status_of("Running", 1)), Some(operation_id));
        let (state, _dir) =
            fence_state(runtime, UserAppOperationKind::StartDeployment, operation_id).await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("settle");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::Failed);
        assert_eq!(
            record.checkpoint["fence_released_evidence"]["kind"],
            "rollout_completed"
        );
        assert!(
            record
                .error_message
                .as_deref()
                .is_some_and(|m| m.contains("fence released")),
            "{:?}",
            record.error_message
        );
    }

    /// 被后续发布取代（活代属别的操作）同样是确定性状态——与 builder 族
    /// "被另一次操作 ensure" 同义，收束并留痕。
    #[tokio::test]
    async fn start_deployment_fence_settles_when_superseded_by_newer_generation() {
        let operation_id = "op-superseded";
        let runtime = FenceRuntime::scenario(Some(status_of("Running", 1)), Some("op-newer"));
        let (state, _dir) =
            fence_state(runtime, UserAppOperationKind::StartDeployment, operation_id).await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("settle");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::Failed);
        assert_eq!(
            record.checkpoint["fence_released_evidence"]["kind"],
            "superseded_by_newer_generation"
        );
    }

    /// 运行态查不到（部署不存在）：证据不足，围栏保持。
    #[tokio::test]
    async fn start_deployment_fence_kept_when_deployment_absent() {
        let operation_id = "op-absent";
        let runtime = FenceRuntime::scenario(None, Some(operation_id));
        let (state, _dir) =
            fence_state(runtime, UserAppOperationKind::StartDeployment, operation_id).await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("scan");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::RecoveryRequired);
        assert!(
            record
                .checkpoint
                .get("superseded_by_live_builder")
                .is_none()
        );
    }

    /// 无证据谓词的 kind（Update）：即使运行态可见也保持围栏——保守面
    /// 不因泛化而扩大。
    #[tokio::test]
    async fn unsupported_kind_fence_kept_even_when_running() {
        let operation_id = "op-update";
        let runtime = FenceRuntime::scenario(Some(status_of("Running", 1)), Some(operation_id));
        let (state, _dir) = fence_state(runtime, UserAppOperationKind::Update, operation_id).await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("scan");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::RecoveryRequired);
    }

    /// builder 族原语义不回退：存活 builder（指纹无关观察）→ 收束。
    #[tokio::test]
    async fn ensure_builder_fence_settles_with_live_builder() {
        let operation_id = "op-builder-live";
        let runtime = FenceRuntime::scenario(None, None);
        *runtime.builder_workload.lock().expect("lock") = Some("rcoder-app-builder-fenced".into());
        let (state, _dir) =
            fence_state(runtime, UserAppOperationKind::EnsureBuilder, operation_id).await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("settle");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::Failed);
        assert_eq!(
            record.checkpoint["fence_released_evidence"]["kind"],
            "live_builder"
        );
    }

    /// builder 缺失：保持围栏（受保护目标未证实）。
    #[tokio::test]
    async fn ensure_builder_fence_kept_when_builder_absent() {
        let operation_id = "op-builder-absent";
        let runtime = FenceRuntime::scenario(None, None);
        let (state, _dir) =
            fence_state(runtime, UserAppOperationKind::EnsureBuilder, operation_id).await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("scan");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::RecoveryRequired);
    }

    /// app 159 事故回归：创建中的瞬时 RecoveryRequired 必须被骑代——3s 后
    /// 恢复核验把操作收束为真终态 Failed，等待方观察到的是真终态而非在
    /// 中间态上提前宣判。修复前必红：立即返回 "ended as RecoveryRequired"。
    #[tokio::test]
    async fn transient_fence_is_ridden_through_to_real_terminal() {
        let operation_id = "op-ride-through";
        let runtime = FenceRuntime::scenario(None, None);
        let (state, _dir) =
            fence_state(runtime, UserAppOperationKind::StartDeployment, operation_id).await;
        let fenced = settled(&state, operation_id).await;
        // 3s 后恢复扫描器的核验收束路径把围栏落为真终态（Failed+证据）。
        let store = state.userapp_store.clone();
        let snapshot = fenced.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            store
                .settle_fenced_operation(&snapshot, &serde_json::json!({"kind": "test"}))
                .await
                .expect("settle");
        });
        let result = wait_record(
            &state.userapp_store,
            &fenced,
            Instant::now() + Duration::from_secs(30),
        )
        .await;
        let error = result.expect_err("terminal failure").to_string();
        assert!(
            error.contains("ended as Failed"),
            "waiter must observe the real terminal, not the transient fence: {error}"
        );
        assert!(
            !error.contains("RecoveryRequired"),
            "waiter must not report the intermediate state as terminal: {error}"
        );
    }

    /// 真围栏（持续不改判）在宽限后仍按终态失败——骑代不变成永久等待。
    #[tokio::test]
    async fn persistent_fence_fails_only_after_grace() {
        let operation_id = "op-persistent-fence";
        let runtime = FenceRuntime::scenario(None, None);
        let (state, _dir) =
            fence_state(runtime, UserAppOperationKind::StartDeployment, operation_id).await;
        let fenced = settled(&state, operation_id).await;
        let started = Instant::now();
        let result = wait_record(
            &state.userapp_store,
            &fenced,
            Instant::now() + Duration::from_secs(45),
        )
        .await;
        let error = result.expect_err("persistent fence must fail").to_string();
        assert!(error.contains("ended as RecoveryRequired"), "{error}");
        assert!(
            started.elapsed() >= RECOVERY_REQUIRED_GRACE,
            "must not fail before the grace window: {:?}",
            started.elapsed()
        );
    }

    /// 已是真终态（Failed）的操作保持立即失败——骑代只对中间态生效。
    #[tokio::test]
    async fn real_terminal_failure_still_fails_fast() {
        let operation_id = "op-already-failed";
        let runtime = FenceRuntime::scenario(None, None);
        let (state, _dir) = fence_state_with_terminal(
            runtime,
            UserAppOperationKind::StartDeployment,
            operation_id,
            UserAppOperationState::Failed,
        )
        .await;
        let failed = settled(&state, operation_id).await;
        let started = Instant::now();
        let result = wait_record(
            &state.userapp_store,
            &failed,
            Instant::now() + Duration::from_secs(10),
        )
        .await;
        let error = result.expect_err("failed operation must error").to_string();
        assert!(error.contains("ended as Failed"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "real terminal must fail fast: {:?}",
            started.elapsed()
        );
    }
}
