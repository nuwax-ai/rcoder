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

/// 发布族共用证据：活代 generation + 运行相位。generation == 本操作 =
/// 本次 rollout 已落地（app 154 事故形态：patch 后零 pod 窗口捕获失败，
/// rollout 事后照常完成）；generation 属后来操作 = 被取代（与 builder 族
/// "被另一次操作 ensure"同义）。维持 env 代次 token 口径，勿改
/// `.metadata.generation`（restart 单写会 bump generation 造成误判）。
async fn observe_generation_fence(
    state: &AppState,
    record: &UserAppOperationRecord,
) -> FenceEvidence {
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

/// per-kind 证据谓词。判定规则统一为：**物理状态确定（无死执行者在途写）**，
/// 而非"操作目标已达成"——围栏的职责是防半应用状态上的并发变更，而围栏
/// 落盘时执行者已携带已知错误返回（`OwnedOperation::fail`），不存在在途
/// 写；后继操作自带 admission/capture/precondition 防线。
///
/// **穷尽 match（禁通配臂）**：24 个 kind 全部显式列出——新增 kind 时编译
/// 器强制在此决定证据语义，避免静默落入"无谓词永久围栏"（09-22 用户纪律）。
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
        UserAppOperationKind::StartDeployment | UserAppOperationKind::RestartDeployment => {
            observe_generation_fence(state, record).await
        }
        // Create/Update：什么都没建出来（无部署痕迹）= no_trace（操作留下的
        // 痕迹全无，可安全关闭，app-166 的 prod 面同族）；有部署痕迹 →
        // 与发布族同一 env 代次口径判定。
        UserAppOperationKind::Create | UserAppOperationKind::Update => {
            match state.runtime().get_deployment_status(&record.app_id).await {
                Ok(None) => FenceEvidence::Observed(serde_json::json!({
                    "kind": "no_trace",
                    "observed_at_us": chrono::Utc::now().timestamp_micros(),
                })),
                Ok(Some(_)) => observe_generation_fence(state, record).await,
                Err(_) => FenceEvidence::Insufficient,
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
        // SetRecyclePolicy：回收注解可读即确定状态（同 Stop 哲学）——
        // desired/observed 双留痕，目标未达成时后继显式操作重放。
        UserAppOperationKind::SetRecyclePolicy => {
            let Some(shared_types::UserAppControlCommand::SetRecyclePolicy { policy }) =
                &record.command
            else {
                return FenceEvidence::Insufficient;
            };
            match state.runtime().get_deployment_status(&record.app_id).await {
                Ok(Some(info)) => {
                    let desired_hit = policy
                        .recycle_enabled
                        .is_none_or(|v| Some(v) == info.recycle_enabled)
                        && policy
                            .idle_timeout_seconds
                            .is_none_or(|v| Some(v) == info.idle_timeout_seconds)
                        && policy
                            .wake_on_traffic
                            .is_none_or(|v| Some(v) == info.wake_on_traffic);
                    FenceEvidence::Observed(serde_json::json!({
                        "kind": if desired_hit { "already_at_desired" } else { "policy_differs" },
                        "desired": policy,
                        "observed": {
                            "recycle_enabled": info.recycle_enabled,
                            "idle_timeout_seconds": info.idle_timeout_seconds,
                            "wake_on_traffic": info.wake_on_traffic,
                        },
                        "observed_at_us": chrono::Utc::now().timestamp_micros(),
                    }))
                }
                Ok(None) | Err(_) => FenceEvidence::Insufficient,
            }
        }
        // 删除族：资源缺席即达成证据（删除中断后残留已不在 = 目标态）；
        // 仍在也属确定状态（留痕 still_present，后继显式删除重试）。
        UserAppOperationKind::DeleteCompute
        | UserAppOperationKind::PurgeResources
        | UserAppOperationKind::DeleteApplication => {
            match state.runtime().get_deployment_status(&record.app_id).await {
                Ok(None) => FenceEvidence::Observed(serde_json::json!({
                    "kind": "absent_confirmed",
                    "observed_at_us": chrono::Utc::now().timestamp_micros(),
                })),
                Ok(Some(info)) => FenceEvidence::Observed(serde_json::json!({
                    "kind": "still_present",
                    "phase": info.phase,
                    "replicas": info.replicas,
                    "observed_at_us": chrono::Utc::now().timestamp_micros(),
                })),
                Err(_) => FenceEvidence::Insufficient,
            }
        }
        // 以下 kind 无只读可判定证据（存储深度/密码远端回执/热部署收敛/
        // 杂项控制）——保守保持围栏，超龄 + holder 死亡由 holder_expired
        // 兜底有界收束。**显式列出（禁通配臂）**：新增 kind 必须在此决定
        // 证据语义。
        UserAppOperationKind::AdoptApplication
        | UserAppOperationKind::HotDeploy
        | UserAppOperationKind::StopBuilder
        | UserAppOperationKind::RestartBuilder
        | UserAppOperationKind::DestroyDevStorage
        | UserAppOperationKind::DestroyProdStorage
        | UserAppOperationKind::ClearDevStorage
        | UserAppOperationKind::ClearProdStorage
        | UserAppOperationKind::ResetDevDatabasePassword
        | UserAppOperationKind::ResetProdDatabasePassword
        | UserAppOperationKind::PrepareProdDatabase => FenceEvidence::Insufficient,
    }
}

/// holder 死亡兜底的收束留痕说明。
const HOLDER_EXPIRED_RELEASE_NOTE: &str =
    "holder expired, outcome unverifiable; retry the operation";
/// 物理状态确定性观察收束的留痕说明。
const DEFINITE_RELEASE_NOTE: &str = "physical state verified definite by recovery scanner";
/// 围栏 holder 死亡兜底的默认超龄门槛（秒）：≫ 操作 deadline + 租约 TTL 60s。
const DEFAULT_FENCE_SETTLE_GRACE_SECS: u64 = 900;

/// validate 判定 → holder 是否已死：`Ok(false)` = 非持有（缺失/TTL 过期/
/// 后端不支持）；`Err(Conflict)` = 身份已变（租约被接管——旧持有者确定性
/// 死亡，不能保守当活）；其余 `Err`（查询/传输失败）保守视作存活。
fn validate_verdict_means_dead(
    outcome: container_runtime_api::ContainerRuntimeResult<bool>,
) -> bool {
    matches!(
        outcome,
        Ok(false) | Err(container_runtime_api::ContainerRuntimeError::Conflict(_))
    )
}

/// holder 死亡兜底判定（围栏有界保证）：**围栏超龄 ∧ 持有者已死**。
///
/// 持有者死亡证明：无租约绑定行（执行者从未取得锁/绑定已被终态清扫），
/// 或绑定的物理租约校验为非持有（TTL 过期=停止续租，或已被接管）。
/// 围栏落盘时执行者已携带已知错误返回（`OwnedOperation::fail`），不存在
/// 正在途写的持有者；查询失败/租约仍持有/未超龄 → 保守保持围栏。
/// 收束一律 Failed（`settle_fenced_operation`），绝不伪造 Succeeded。
async fn holder_expired(state: &AppState, record: &UserAppOperationRecord) -> bool {
    let grace_secs = state
        .config
        .fence_settle_grace_secs
        .unwrap_or(DEFAULT_FENCE_SETTLE_GRACE_SECS);
    let grace_us = i64::try_from(grace_secs.saturating_mul(1_000_000)).unwrap_or(i64::MAX);
    let now_us = chrono::Utc::now().timestamp_micros();
    // 计龄从 created_at 起算（记录仅暴露 created_at；created ≤ updated，
    // 保守取更长的等待）。
    if now_us.saturating_sub(record.created_at.timestamp_micros()) < grace_us {
        return false;
    }
    match state
        .userapp_store
        .get_operation_lease(&record.app_id, &record.operation_id)
        .await
    {
        Ok(None) => true,
        Ok(Some(binding)) => {
            let outcome = state
                .runtime()
                .validate_app_operation_receipt(&binding.context, &binding.receipt)
                .await;
            validate_verdict_means_dead(outcome)
        }
        Err(error) => {
            tracing::warn!(
                operation_id = %record.operation_id,
                app_id = %record.app_id,
                %error,
                "Fence kept: holder death check failed to read lease binding"
            );
            false
        }
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
    // 只读观察整体限时：kube 请求无总超时（kube-rs 默认 read_timeout=None），
    // 一个 stall 连接会把恢复任务永久挂死，占满扫描器 8 槽后瘫痪整个
    // 恢复管线（0.1.288 线上 20 围栏零收束的根因形态）。超时=证据不足，
    // 保守保持围栏，下一扫描周期重试。
    let observation = tokio::time::timeout(
        Duration::from_secs(10),
        observe_fence_evidence(state, &record),
    )
    .await;
    let evidence = match observation {
        Ok(FenceEvidence::Observed(evidence)) => evidence,
        Err(_) => {
            tracing::warn!(
                operation_id = %record.operation_id,
                app_id = %record.app_id,
                "Fence kept: evidence observation timed out (stalled runtime call)"
            );
            return Ok(());
        }
        Ok(FenceEvidence::Insufficient) => {
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
            // holder 死亡兜底：无据可查但持有者已死（无绑定/租约过期）且
            // 围栏超龄 → 有界收束，不再永久占受理 slot（app-166 事故类：
            // 创建报错落围栏后什么都没建出来，live 证据谓词永不满足）。
            if holder_expired(state, &record).await {
                let evidence = serde_json::json!({
                    "kind": "holder_expired",
                    "checked_at_us": chrono::Utc::now().timestamp_micros(),
                });
                state
                    .userapp_store
                    .settle_fenced_operation(&record, &evidence, HOLDER_EXPIRED_RELEASE_NOTE)
                    .await?;
                tracing::warn!(
                    operation_id = %record.operation_id,
                    app_id = %record.app_id,
                    kind = ?record.kind,
                    "Fenced operation settled as Failed: holder expired, outcome unverifiable"
                );
            }
            return Ok(());
        }
    };
    // 经 store 的 sanctioned 终态化路径（内部 Running 跳转满足状态机独占
    // 门，纯记账不授权运行时工作）——直接 advance(Failed) 会被
    // domain::advance 的 RecoveryRequired 转移门拒绝（f49b594d 潜伏 bug：
    // settler 从未真正收束过任何围栏）。
    state
        .userapp_store
        .settle_fenced_operation(&record, &evidence, DEFINITE_RELEASE_NOTE)
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

    /// 可控运行态：deployment 相位/副本 + 活代 generation + builder 观察 +
    /// 物理租约持有状态（validate_app_operation_receipt）。
    #[derive(Default)]
    pub(crate) struct FenceRuntime {
        status: Mutex<Option<Option<DeploymentStatus>>>,
        generation: Mutex<Option<String>>,
        pub(crate) builder_workload: Mutex<Option<String>>,
        pub(crate) receipt_alive: Mutex<bool>,
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
                receipt_alive: Mutex::new(false),
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
            reason: None,
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

        async fn validate_app_operation_receipt(
            &self,
            _context: &shared_types::UserAppExecutionContext,
            _receipt: &shared_types::UserAppOperationLeaseReceipt,
        ) -> ContainerRuntimeResult<bool> {
            Ok(*self.receipt_alive.lock().expect("lock"))
        }
    }

    pub(crate) async fn fence_state(
        runtime: Arc<FenceRuntime>,
        kind: UserAppOperationKind,
        operation_id: &str,
    ) -> (Arc<AppState>, tempfile::TempDir) {
        fence_state_custom(runtime, kind, operation_id, None, None).await
    }

    /// 带围栏收束 grace 覆盖与可选租约绑定的围栏夹具（holder 死亡兜底反例用）。
    pub(crate) async fn fence_state_custom(
        runtime: Arc<FenceRuntime>,
        kind: UserAppOperationKind,
        operation_id: &str,
        fence_settle_grace_secs: Option<u64>,
        bind_receipt: Option<shared_types::UserAppOperationLeaseReceipt>,
    ) -> (Arc<AppState>, tempfile::TempDir) {
        fence_state_with_terminal(
            runtime,
            kind,
            operation_id,
            UserAppOperationState::RecoveryRequired,
            fence_settle_grace_secs,
            bind_receipt,
        )
        .await
    }

    async fn fence_state_with_terminal(
        runtime: Arc<FenceRuntime>,
        kind: UserAppOperationKind,
        operation_id: &str,
        terminal: UserAppOperationState,
        fence_settle_grace_secs: Option<u64>,
        bind_receipt: Option<shared_types::UserAppOperationLeaseReceipt>,
    ) -> (Arc<AppState>, tempfile::TempDir) {
        let metadata_dir = tempfile::tempdir().expect("metadata directory");
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &metadata_dir.path().join("userapp.turso.db"),
            )
            .await
            .expect("store"),
        );
        let (state, _keep) = fence_state_on_store(
            runtime,
            kind,
            operation_id,
            terminal,
            store,
            fence_settle_grace_secs,
            bind_receipt,
        )
        .await;
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
            None,
            None,
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
        fence_settle_grace_secs: Option<u64>,
        bind_receipt: Option<shared_types::UserAppOperationLeaseReceipt>,
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
            config: crate::config::AppConfig {
                fence_settle_grace_secs,
                ..Default::default()
            },
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
        // 命令↔kind 严格对应（权威映射 = UserAppControlCommand::kind() 的
        // 穷尽 match）；EnsureBuilder/AdoptBuilder/AdoptApplication/HotDeploy
        // 无对应命令变体传 None。**禁止通配臂**：新增 kind 必须在此显式补
        // 映射，否则 admit 的命令↔摘要校验会拒绝（09-22 用户纪律：穷尽
        // match，`_` 会静默吞掉新增变体）。仅携带 input_digest 的命令附带
        // 执行输入。
        let input = shared_types::UserAppExecutionInput::new("{}".into());
        let digest = input.digest();
        let (command, with_input) = match kind {
            UserAppOperationKind::EnsureBuilder
            | UserAppOperationKind::AdoptBuilder
            | UserAppOperationKind::AdoptApplication
            | UserAppOperationKind::HotDeploy => (None, false),
            UserAppOperationKind::StartDeployment => (
                Some(UserAppControlCommand::Deploy {
                    restart: false,
                    input_digest: digest,
                }),
                true,
            ),
            UserAppOperationKind::RestartDeployment => (
                Some(UserAppControlCommand::Deploy {
                    restart: true,
                    input_digest: digest,
                }),
                true,
            ),
            UserAppOperationKind::Create => (
                Some(UserAppControlCommand::Create {
                    input_digest: digest,
                }),
                true,
            ),
            UserAppOperationKind::Update => (
                Some(UserAppControlCommand::Update {
                    input_digest: digest,
                }),
                true,
            ),
            UserAppOperationKind::Start => {
                (Some(UserAppControlCommand::Start { traffic: false }), false)
            }
            UserAppOperationKind::Restart => (Some(UserAppControlCommand::Restart), false),
            UserAppOperationKind::Stop => (
                Some(UserAppControlCommand::Stop {
                    wake_on_traffic: true,
                }),
                false,
            ),
            UserAppOperationKind::SetRecyclePolicy => (
                Some(UserAppControlCommand::SetRecyclePolicy {
                    policy: shared_types::UserAppRuntimePolicy {
                        recycle_enabled: Some(true),
                        idle_timeout_seconds: Some(7200),
                        wake_on_traffic: Some(true),
                    },
                }),
                false,
            ),
            UserAppOperationKind::DeleteCompute => (
                Some(UserAppControlCommand::DeleteResources {
                    purge: false,
                    expected_resource_version: None,
                }),
                false,
            ),
            UserAppOperationKind::PurgeResources => (
                Some(UserAppControlCommand::DeleteResources {
                    purge: true,
                    expected_resource_version: None,
                }),
                false,
            ),
            UserAppOperationKind::DeleteApplication => {
                (Some(UserAppControlCommand::DeleteApplication), false)
            }
            UserAppOperationKind::DestroyDevStorage => (
                Some(UserAppControlCommand::DestroyStorage { production: false }),
                false,
            ),
            UserAppOperationKind::DestroyProdStorage => (
                Some(UserAppControlCommand::DestroyStorage { production: true }),
                false,
            ),
            UserAppOperationKind::ClearDevStorage => (
                Some(UserAppControlCommand::ClearStorage { production: false }),
                false,
            ),
            UserAppOperationKind::ClearProdStorage => (
                Some(UserAppControlCommand::ClearStorage { production: true }),
                false,
            ),
            UserAppOperationKind::ResetDevDatabasePassword => (
                Some(UserAppControlCommand::ResetDatabasePassword {
                    production: false,
                    username: "app".into(),
                }),
                false,
            ),
            UserAppOperationKind::ResetProdDatabasePassword => (
                Some(UserAppControlCommand::ResetDatabasePassword {
                    production: true,
                    username: "app".into(),
                }),
                false,
            ),
            UserAppOperationKind::PrepareProdDatabase => {
                (Some(UserAppControlCommand::PrepareProdDatabase), false)
            }
            UserAppOperationKind::StopBuilder => (Some(UserAppControlCommand::StopBuilder), false),
            UserAppOperationKind::RestartBuilder => {
                (Some(UserAppControlCommand::RestartBuilder), false)
            }
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
                with_input.then_some(&input),
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
        if let Some(receipt) = &bind_receipt {
            store
                .bind_operation_lease(
                    &shared_types::UserAppExecutionContext {
                        app_id: "fenced".into(),
                        lifecycle_id: lifecycle.lifecycle_id.clone(),
                        operation_id: operation_id.into(),
                        executor_id: "executor-fenced".into(),
                        request_fingerprint: "cd".repeat(32),
                    },
                    receipt,
                )
                .await
                .expect("bind lease");
        }
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
                // 失败/围栏转移保持已记录 checkpoint（domain 对删除/存储族
                // 有"必须保持证据"校验，真实执行器 fail 亦保点）。
                checkpoint: running.checkpoint.clone(),
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

    /// 无证据谓词的 kind：即使运行态可见也保持围栏——保守面不因泛化而
    /// 扩大（超龄+holder 死亡由兜底收束，见 holder_expired 系列）。
    /// 注：本测试原以 Update 为靶，Step B 给 Create/Update 补谓词后按批准
    /// 需求换到仍无谓词的存储族 kind（测试预期随需求偏移，语义保持）。
    #[tokio::test]
    async fn unsupported_kind_fence_kept_even_when_running() {
        let operation_id = "op-destroy-storage";
        let runtime = FenceRuntime::scenario(Some(status_of("Running", 1)), Some(operation_id));
        let (state, _dir) = fence_state(
            runtime,
            UserAppOperationKind::DestroyDevStorage,
            operation_id,
        )
        .await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("scan");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::RecoveryRequired);
    }

    /// Step B 反例（修复前必挂）：删除中断后部署已缺席 = 达成证据
    /// （同 Stop 哲学），必须收束留痕而不是永久围栏。
    #[tokio::test]
    async fn delete_compute_fence_settles_when_deployment_absent() {
        let operation_id = "op-delete-absent";
        let runtime = FenceRuntime::scenario(None, None);
        let (state, _dir) =
            fence_state(runtime, UserAppOperationKind::DeleteCompute, operation_id).await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("settle");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::Failed);
        assert_eq!(
            record.checkpoint["fence_released_evidence"]["kind"], "absent_confirmed",
            "{:?}",
            record.checkpoint["fence_released_evidence"]
        );
    }

    /// Step B 反例（修复前必挂）：Create 报错且什么都没建出来 → no_trace
    /// 收束（app-166 同族的 prod 面）。
    #[tokio::test]
    async fn create_fence_settles_no_trace_when_nothing_created() {
        let operation_id = "op-create-no-trace";
        let runtime = FenceRuntime::scenario(None, None);
        let (state, _dir) = fence_state(runtime, UserAppOperationKind::Create, operation_id).await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("settle");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::Failed);
        assert_eq!(
            record.checkpoint["fence_released_evidence"]["kind"], "no_trace",
            "{:?}",
            record.checkpoint["fence_released_evidence"]
        );
    }

    /// Step B 反例（修复前必挂）：SetRecyclePolicy 的注解可读即确定状态
    /// ——已达 desired 收束留痕（policy 观察值入证据）。
    #[tokio::test]
    async fn set_recycle_policy_fence_settles_with_observed_policy() {
        let operation_id = "op-recycle-policy";
        let mut status = status_of("Running", 1);
        status.recycle_enabled = Some(true);
        status.idle_timeout_seconds = Some(7200);
        status.wake_on_traffic = Some(true);
        let runtime = FenceRuntime::scenario(Some(status), None);
        let (state, _dir) = fence_state(
            runtime,
            UserAppOperationKind::SetRecyclePolicy,
            operation_id,
        )
        .await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("settle");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::Failed);
        assert_eq!(
            record.checkpoint["fence_released_evidence"]["kind"], "already_at_desired",
            "{:?}",
            record.checkpoint["fence_released_evidence"]
        );
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

    /// Step A 反例（修复前必挂，app-166 事故类）：创建报错落围栏后什么都没
    /// 建出来（无 workload、无租约绑定）——持有者已死（0 绑定）且超龄
    /// （grace=0）时必须兜底收束为 Failed，不再永久占受理 slot。
    /// 判定表锁：被接管（Err(Conflict)）与非持有（Ok(false)）都是 holder
    /// 死亡证明；查询/传输失败保守视作存活。
    #[test]
    fn validate_verdict_dead_mapping() {
        assert!(validate_verdict_means_dead(Ok(false)));
        assert!(validate_verdict_means_dead(Err(
            ContainerRuntimeError::Conflict("taken over".into())
        )));
        assert!(!validate_verdict_means_dead(Ok(true)));
        assert!(!validate_verdict_means_dead(Err(
            ContainerRuntimeError::K8sError("io".into())
        )));
    }

    #[tokio::test]
    async fn holder_expired_fence_without_binding_settles() {
        let operation_id = "op-holder-expired";
        let runtime = FenceRuntime::scenario(None, None); // 无 workload → 谓词 Insufficient
        let (state, _dir) = fence_state_custom(
            runtime,
            UserAppOperationKind::EnsureBuilder,
            operation_id,
            Some(0),
            None,
        )
        .await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("settle");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::Failed);
        assert_eq!(
            record.checkpoint["fence_released_evidence"]["kind"], "holder_expired",
            "evidence 必须留痕 holder 死亡判定: {:?}",
            record.checkpoint["fence_released_evidence"]
        );
        assert!(
            record
                .error_message
                .as_deref()
                .is_some_and(|m| m.contains("holder expired")),
            "{:?}",
            record.error_message
        );
        // 端到端：slot 已释放——同 app 同 scope 新受理必须 Accepted。
        let admitted = state
            .userapp_store
            .admit_with_input(
                &UserAppAdmission {
                    app_id: "fenced".into(),
                    lifecycle_id: Some(record.lifecycle_id.clone()),
                    operation_id: format!("{operation_id}-retry"),
                    request_id: Some(format!("req-{operation_id}-retry")),
                    request_fingerprint: "ef".repeat(32),
                    kind: UserAppOperationKind::EnsureBuilder,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                },
                None,
            )
            .await
            .expect("admit after settle");
        assert!(
            matches!(admitted, UserAppAdmissionOutcome::Accepted(_)),
            "收束后同 scope 必须可受理: {admitted:?}"
        );
    }

    /// 锁：围栏未超龄（默认 grace）→ 即使无绑定也保持围栏——兜底只对
    /// 超龄围栏生效，给在途收尾留出宽限。
    #[tokio::test]
    async fn fresh_fence_without_binding_keeps() {
        let operation_id = "op-fresh-fence";
        let runtime = FenceRuntime::scenario(None, None);
        let (state, _dir) = fence_state_custom(
            runtime,
            UserAppOperationKind::EnsureBuilder,
            operation_id,
            None,
            None,
        )
        .await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("scan");
        let record = settled(&state, operation_id).await;
        assert_eq!(record.state, UserAppOperationState::RecoveryRequired);
    }

    /// 锁：绑定行存在且物理租约仍持有（validate=true）→ holder 未死，
    /// 即使超龄也保持围栏——兜底不得从活持有者手里收束。
    #[tokio::test]
    async fn live_lease_binding_keeps_fence() {
        let operation_id = "op-live-lease";
        let runtime = FenceRuntime::scenario(None, None);
        *runtime.receipt_alive.lock().expect("lock") = true;
        let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type: ServiceType::UserappBuilder,
            namespace: "test-ns".into(),
            name: format!("rcoder-operation-builder-{operation_id}"),
            uid: "lease-uid".into(),
            resource_version: "1".into(),
            token: "lease-token".into(),
        };
        let (state, _dir) = fence_state_custom(
            runtime,
            UserAppOperationKind::EnsureBuilder,
            operation_id,
            Some(0),
            Some(receipt),
        )
        .await;
        let snapshot = settled(&state, operation_id).await;
        reconcile_fenced_ensure(&state, &snapshot)
            .await
            .expect("scan");
        let record = settled(&state, operation_id).await;
        assert_eq!(
            record.state,
            UserAppOperationState::RecoveryRequired,
            "活租约必须保持围栏"
        );
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
                .settle_fenced_operation(
                    &snapshot,
                    &serde_json::json!({"kind": "test"}),
                    "test fixture settle",
                )
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
            None,
            None,
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
