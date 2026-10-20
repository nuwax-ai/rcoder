//! Same-operation handoff before a durable Stopping/Starting intent exists.
use super::{compute_control, dev_cleanup::BuilderOperation};
use crate::app_state::AppState;
use anyhow::{Context, Result, anyhow};
use shared_types::*;
use std::time::Duration;

mod previous_owner;
mod superseded;

#[cfg(test)]
pub(crate) struct UnheldComputeFixture;
#[cfg(test)]
impl container_runtime_api::ComputeHandoffGuard for UnheldComputeFixture {}

// A prepared lease cannot authorize a container write. Cancellation here may
// release it, including an uncertain DB bind: the next executor still checks CAS.
struct Preparing(Option<Box<dyn PreparedComputeLease>>);
impl Drop for Preparing {
    fn drop(&mut self) {
        if let Some(lease) = self.0.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                        if let Err(error) = lease.release().await {
                            tracing::warn!(%error, "Prepared compute lease awaits exact-receipt recovery");
                        }
                    });
                }
                Err(error) => {
                    tracing::warn!(%error, "Prepared compute lease cleanup has no runtime")
                }
            }
        }
    }
}

pub(super) async fn prepare_and_bind(
    state: &AppState,
    record: &mut ComputeControlRecord,
    identity: &ComputeExecutorIdentity,
) -> Result<BuilderOperation> {
    state.userapp_store.check_compute_executor(identity).await?;
    let context = record.execution_context().map_err(anyhow::Error::msg)?;
    let mut preparing = Preparing(Some(
        state
            .runtime()
            .prepare_compute_operation(&context, identity.scope)
            .await?,
    ));
    let lease = preparing
        .0
        .as_mut()
        .ok_or_else(|| anyhow!("Prepared lease missing"))?;
    let receipt = lease
        .receipt()
        .ok_or_else(|| anyhow!("Prepared lease has no receipt"))?;
    *record = state
        .userapp_store
        .bind_compute_lease(identity, &receipt)
        .await?;
    if record.action == ComputeControlAction::Restart {
        state
            .app_service
            .verify_recovered_storage(&record.app_id, identity.scope)
            .await?;
    }
    lease.activate().await.map_err(anyhow::Error::msg)?;
    // An early recovery may have revoked us while binding/activating. No runtime
    // mutations are allowed until the normal chain persists its next stage.
    state.userapp_store.check_compute_executor(identity).await?;
    Ok(BuilderOperation::new(preparing.0.take().ok_or_else(
        || anyhow!("Prepared lease already consumed"),
    )?))
}

fn identity(record: &ComputeControlRecord) -> Result<ComputeExecutorIdentity> {
    Ok(ComputeExecutorIdentity {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        scope: record.scope,
        operation_id: record.operation_id.clone(),
        generation: record.generation,
        executor_id: record
            .executor_id
            .clone()
            .ok_or_else(|| anyhow!("Compute executor missing"))?,
    })
}

/// A scanner schedules one bounded inspection; execution after a successful CAS
/// uses the ordinary coordinator with its existing per-stage budgets.
pub(super) async fn recover_and_execute(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<()> {
    if let Some(pending) = prepare_resume(state, snapshot).await? {
        compute_control::execute_pending(state, pending).await?;
    }
    Ok(())
}

pub(super) async fn prepare_resume(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<Option<ComputeControlRecord>> {
    prepare_resume_with_budget(state, snapshot, Duration::from_secs(30)).await
}

pub(super) async fn prepare_resume_with_budget(
    state: &AppState,
    snapshot: &ComputeControlRecord,
    budget: Duration,
) -> Result<Option<ComputeControlRecord>> {
    let deadline = tokio::time::Instant::now() + budget;
    let handoff = if snapshot.state == ComputeControlState::Running {
        match tokio::time::timeout_at(deadline, prepare_handoff(state, snapshot)).await {
            Ok(Ok(Some(prepared))) => Some(prepared),
            Ok(Ok(None)) => return Ok(None),
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(anyhow!("Early compute handoff observation timed out")),
        }
    } else {
        None
    };
    let original_absent = handoff
        .as_ref()
        .is_some_and(|prepared| prepared.original_absent);
    let wait_for_late_prepare = snapshot.state == ComputeControlState::Running;
    let mut current = match state
        .userapp_store
        .reserve_compute_drain_recovery(snapshot)
        .await
    {
        Ok(record) => record,
        Err(UserAppStoreError::VersionConflict) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // Dropping only unlocks/stops renewal. It never clears a marker or deletes
    // an object, including a CAS loser whose original executor advanced first.
    drop(handoff);
    let result = tokio::time::timeout_at(
        deadline,
        inspect_and_resume(state, &mut current, original_absent, wait_for_late_prepare),
    )
    .await;
    let (code, message) = match result {
        Ok(Ok(Resume::Ready(pending))) => return Ok(Some(*pending)),
        Ok(Ok(Resume::Converged(code))) => {
            tracing::info!(
                code,
                operation_id = %current.operation_id,
                app_id = %current.app_id,
                "Recovered orphan compute operation converged to terminal"
            );
            return Ok(None);
        }
        Ok(Ok(Resume::Deferred(code, message))) => (code, message),
        Ok(Err(error))
            if matches!(
                error.downcast_ref::<UserAppStoreError>(),
                Some(UserAppStoreError::VersionConflict)
            ) =>
        {
            return Ok(None);
        }
        Ok(Err(error)) => ("COMPUTE_OBSERVATION_FAILED", format!("{error:#}")),
        Err(_) => (
            "COMPUTE_OBSERVATION_FAILED",
            "Early compute recovery inspection timed out; captured identity retained".into(),
        ),
    };
    match state
        .userapp_store
        .record_compute_drain_problem(&current, code, &message)
        .await
    {
        Ok(_) | Err(UserAppStoreError::VersionConflict) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

struct PreparedHandoff {
    // Lifetime ownership retained across the durable reservation await.
    _guard: Option<Box<dyn container_runtime_api::ComputeHandoffGuard>>,
    original_absent: bool,
}

async fn prepare_handoff(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<Option<PreparedHandoff>> {
    use container_runtime_api::ComputeHandoff;
    let context = snapshot.execution_context().map_err(anyhow::Error::msg)?;
    loop {
        // Runtime exclusion is not authority to replace another store revision.
        if state
            .userapp_store
            .get_compute_control(&snapshot.app_id, &snapshot.operation_id)
            .await?
            .as_ref()
            != Some(snapshot)
        {
            return Ok(None);
        }
        match state
            .runtime()
            .prepare_compute_handoff(&context, snapshot.scope, snapshot.lease.as_ref())
            .await?
        {
            ComputeHandoff::Held => return Ok(None),
            ComputeHandoff::Vacant { original_absent } => {
                return Ok(Some(PreparedHandoff {
                    _guard: None,
                    original_absent,
                }));
            }
            ComputeHandoff::Acquired {
                guard,
                original_absent,
            } => {
                return Ok(Some(PreparedHandoff {
                    _guard: Some(guard),
                    original_absent,
                }));
            }
            ComputeHandoff::Deferred(inspection) => {
                let released = match *inspection {
                    ComputeLeaseInspection::Discovered { attempt, .. } => {
                        return prepare_fenced_attempt_handoff(state, snapshot, &attempt).await;
                    }
                    ComputeLeaseInspection::PreviousOwner { receipt, owner } => {
                        previous_owner::release(state, snapshot, &owner, &receipt).await?
                    }
                    ComputeLeaseInspection::PreviousOperation {
                        receipt,
                        operation_id,
                    } => {
                        previous_owner::release_operation(state, snapshot, &operation_id, &receipt)
                            .await?
                    }
                    ComputeLeaseInspection::Absent
                    | ComputeLeaseInspection::Held
                    | ComputeLeaseInspection::Releasable(_)
                    | ComputeLeaseInspection::IdentityChanged(_) => false,
                };
                if !released {
                    return Ok(None);
                }
            }
        }
    }
}

async fn prepare_fenced_attempt_handoff(
    state: &AppState,
    snapshot: &ComputeControlRecord,
    attempt: &UserAppExecutionContext,
) -> Result<Option<PreparedHandoff>> {
    attempt
        .validate_identity(&snapshot.app_id)
        .map_err(anyhow::Error::msg)?;
    if attempt.lifecycle_id != snapshot.lifecycle_id
        || attempt.operation_id != snapshot.operation_id
        || attempt.request_fingerprint != snapshot.request_fingerprint
        || snapshot.executor_id.as_deref() == Some(attempt.executor_id.as_str())
    {
        return Ok(None);
    }
    let old = ComputeExecutorIdentity {
        app_id: attempt.app_id.clone(),
        lifecycle_id: attempt.lifecycle_id.clone(),
        scope: snapshot.scope,
        operation_id: attempt.operation_id.clone(),
        generation: snapshot.generation,
        executor_id: attempt.executor_id.clone(),
    };
    match state.userapp_store.check_compute_executor(&old).await {
        Err(UserAppStoreError::VersionConflict) => {}
        Ok(()) => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    if state
        .userapp_store
        .get_compute_control(&snapshot.app_id, &snapshot.operation_id)
        .await?
        .as_ref()
        != Some(snapshot)
    {
        return Ok(None);
    }
    match state
        .runtime()
        .prepare_compute_handoff(attempt, snapshot.scope, None)
        .await?
    {
        container_runtime_api::ComputeHandoff::Held
        | container_runtime_api::ComputeHandoff::Deferred(_) => Ok(None),
        container_runtime_api::ComputeHandoff::Vacant { .. } => Ok(Some(PreparedHandoff {
            _guard: None,
            original_absent: true,
        })),
        container_runtime_api::ComputeHandoff::Acquired { guard, .. } => {
            Ok(Some(PreparedHandoff {
                _guard: Some(guard),
                original_absent: true,
            }))
        }
    }
}

enum Resume {
    Ready(Box<ComputeControlRecord>),
    Deferred(&'static str, String),
    /// 恢复器已把原操作按终态收口（image-roll 目标缺席 → Failed/ERR_RUNTIME_ABSENT）：
    /// 不再续跑、不记 problem，围栏随终态自然释放，由业务 start 重建运行时。
    Converged(&'static str),
}

async fn inspect_and_resume(
    state: &AppState,
    current: &mut ComputeControlRecord,
    original_absent: bool,
    wait_for_late_prepare: bool,
) -> Result<Resume> {
    if !compute_control::drain_previous_once(state, current, &identity(current)?).await? {
        return Ok(Resume::Deferred(
            "COMPUTE_DRAIN_PENDING",
            "Interrupted operation has not produced final effects evidence".into(),
        ));
    }
    let context = current.execution_context().map_err(anyhow::Error::msg)?;
    let inspection = loop {
        let inspection = state
            .runtime()
            .inspect_compute_drain_lease(
                &context,
                current.scope,
                if original_absent {
                    None
                } else {
                    current.lease.as_ref()
                },
            )
            .await?;
        if inspection == ComputeLeaseInspection::Held && wait_for_late_prepare {
            // A pre-CAS prepare RPC may arrive late, but its old bind is now
            // fenced. Observe its real FD/handle leaving within the SAME budget.
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        break inspection;
    };
    match inspection {
        ComputeLeaseInspection::Absent => {}
        ComputeLeaseInspection::Held => {
            return Ok(Resume::Deferred(
                "COMPUTE_LEASE_HELD",
                "Original compute lease is still physically held; retry inspection after its holder exits".into(),
            ));
        }
        ComputeLeaseInspection::IdentityChanged(reason) => {
            return Ok(Resume::Deferred("COMPUTE_LEASE_IDENTITY_CHANGED", reason));
        }
        ComputeLeaseInspection::Discovered { receipt, attempt } => {
            anyhow::ensure!(
                current.lease.is_none() || original_absent,
                "Discovered lease cannot replace a registered receipt"
            );
            if current.lease.is_none() {
                *current = state
                    .userapp_store
                    .capture_compute_drain_lease(current, &receipt, &attempt)
                    .await?;
            }
            state
                .runtime()
                .release_app_operation_receipt(&attempt, &receipt)
                .await
                .context("Release recovered prepared compute lease")?;
        }
        ComputeLeaseInspection::PreviousOwner { receipt, owner } => {
            if !previous_owner::release(state, current, &owner, &receipt).await? {
                return Ok(Resume::Deferred(
                    "COMPUTE_PREVIOUS_OWNER_PENDING",
                    "Previous ordinary lease owner is not confirmed terminal and fenced".into(),
                ));
            }
        }
        ComputeLeaseInspection::PreviousOperation {
            receipt,
            operation_id,
        } => {
            if !previous_owner::release_operation(state, current, &operation_id, &receipt).await? {
                return Ok(Resume::Deferred(
                    "COMPUTE_PREVIOUS_OWNER_PENDING",
                    "Previous ordinary marker owner is not confirmed terminal and fenced".into(),
                ));
            }
        }
        ComputeLeaseInspection::Releasable(receipt) => {
            if current.lease.is_none() {
                *current = state
                    .userapp_store
                    .capture_compute_drain_lease(current, &receipt, &context)
                    .await?;
            } else {
                anyhow::ensure!(
                    current.lease.as_ref() == Some(&receipt) || original_absent,
                    "Runtime returned a different captured compute lease"
                );
            }
            state
                .runtime()
                .release_app_operation_receipt(&context, &receipt)
                .await
                .context("Release original early compute lease")?;
        }
    }
    // Image-roll restart 的后半段假设"滚动目标仍在"。若前代容器/Deployment 已被
    // 删除（排干已事实完成、无目标可滚），续跑只会永远撞 ContainerNotFound 并把
    // 操作卡在 recovery_required。续跑前用类型化的 app_compute_absent 探测
    // （缺席必须被正向证实；探测错误原样冒泡，绝不当缺席处理）：缺席即按
    // Failed/ERR_RUNTIME_ABSENT 终态收口，围栏释放，由业务 start 重建运行时。
    // 仅 Prod 域: app_compute_absent 探测的是 rcoder-app-{id} 运行时, 对 Dev(builder)
    // 重启无意义且会误伤(Dev 应用常无 prod 运行时 → 恒真)。
    if current.scope == UserAppOperationScope::Prod
        && is_image_roll_restart(current)
        && state.runtime().app_compute_absent(&context).await?
    {
        converge_absent_image_roll(state, current).await?;
        return Ok(Resume::Converged(ERR_RUNTIME_ABSENT));
    }
    let pending = state
        .userapp_store
        .resume_released_compute_drain(current, current.lease.as_ref())
        .await?;
    Ok(Resume::Ready(Box::new(pending)))
}

/// Observe a superseded early controller without admitting a new operation.
pub(super) async fn recover_superseded_early(
    state: &AppState,
    snapshot: &ComputeControlRecord,
) -> Result<()> {
    superseded::recover(state, snapshot).await
}

/// The successor retains the original interrupted ID, so late prepared lease
/// creation is reconciled along the same original Stop/Restart entry point.
pub(super) async fn observe_unsubmitted_predecessor(
    state: &AppState,
    previous: &ComputeControlRecord,
    current: &ComputeControlRecord,
) -> Result<bool> {
    if !previous.has_unsubmitted_compute_drain_ack() {
        return Ok(true);
    }
    anyhow::ensure!(
        previous.app_id == current.app_id
            && previous.lifecycle_id == current.lifecycle_id
            && previous.scope == current.scope,
        "Interrupted prepared compute scope differs from its successor"
    );
    if let Some(receipt) = &current.lease {
        let context = current.execution_context().map_err(anyhow::Error::msg)?;
        if state
            .runtime()
            .validate_app_operation_receipt(&context, receipt)
            .await?
        {
            // Exact current scope mutex cannot simultaneously be occupied by
            // the predecessor. The stored receipt alone never proves this.
            return Ok(true);
        }
    }
    superseded::release_prepared_mutex(state, previous).await
}

/// 孤儿 image-roll restart 判定：restart 且 checkpoint 记录了 restart_image_roll。
pub(super) fn is_image_roll_restart(record: &ComputeControlRecord) -> bool {
    record.action == ComputeControlAction::Restart
        && record
            .checkpoint
            .get("restart_image_roll")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

/// 把"滚动目标已缺席"的 recovery_required 原操作按 Failed 终态收口。CAS 失败
/// （VersionConflict）表示另一 replica 已处理，静默即可。前代容器已删 = 排干已
/// 事实完成且镜像滚动无目标可滚；不伪造成功，围栏随终态释放，由业务 start
/// 携带新配置重建运行时。保留 checkpoint 证据并写 terminal_at（存储原语
/// fail_unrecoverable_compute_control）。
pub(super) async fn converge_absent_image_roll(
    state: &AppState,
    record: &ComputeControlRecord,
) -> Result<()> {
    const MESSAGE: &str = concat!(
        "image-roll target runtime is absent; previous generation was already drained; ",
        "resubmit start to recreate the runtime"
    );
    anyhow::ensure!(
        record.state == ComputeControlState::RecoveryRequired,
        "absent-target convergence only settles recovery_required records, got {}",
        record
            .state
            .as_progress_storage_value()
            .unwrap_or("unknown")
    );
    let result = state
        .userapp_store
        .fail_unrecoverable_compute_control(record, ERR_RUNTIME_ABSENT, MESSAGE)
        .await
        .map(|_| ());
    match result {
        Ok(()) | Err(UserAppStoreError::VersionConflict) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod image_roll_absent_tests {
    use super::*;
    use crate::userapp_builder::creation::fence_settler_tests::{FenceRuntime, runtime_state};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    /// 复现生产孤儿形态: prod 域 restart, recovery_required/draining_previous,
    /// checkpoint 仅含 restart_image_roll，无 interrupted_operations；保留租约
    /// 回执，但运行时的原租约已缺席。
    async fn stuck_record(
        checkpoint: serde_json::Value,
        target_absent: bool,
        scope: UserAppOperationScope,
    ) -> (
        Arc<AppState>,
        ComputeControlRecord,
        Arc<FenceRuntime>,
        tempfile::TempDir,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("absent-roll.db"),
            )
            .await
            .unwrap(),
        );
        let app = store.ensure_identity("absentroll").await.unwrap();
        let pending = store
            .admit_compute_control(&ComputeControlRequest {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id.clone(),
                scope,
                operation_id: "orphanroll".into(),
                request_id: "orphanrequest".into(),
                request_fingerprint: "f".repeat(64),
                action: ComputeControlAction::Restart,
                restart_image_roll: true,
                builder_startup_mode: BuilderStartupMode::PreserveBusinessIntent,
            })
            .await
            .unwrap();
        let identity = ComputeExecutorIdentity {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: pending.scope,
            operation_id: pending.operation_id.clone(),
            generation: pending.generation,
            executor_id: "lostexecutor".into(),
        };
        store
            .claim_compute_control(&identity, pending.revision)
            .await
            .unwrap();
        // 与生产卡死形态一致: 绑定一个 Docker FD 租约(drain 恢复分类依赖租约证据)。
        let lock_path = directory.path().join("original.lock");
        let fd = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        fd.try_lock().unwrap();
        use std::os::unix::fs::MetadataExt as _;
        let metadata = fd.metadata().unwrap();
        let receipt = UserAppOperationLeaseReceipt::Docker {
            service_type: compute_lease_family(scope).unwrap(),
            device: metadata.dev(),
            inode: metadata.ino(),
            token: identity.executor_id.clone(),
        };
        let claimed = store.bind_compute_lease(&identity, &receipt).await.unwrap();
        drop(fd);
        let record = store
            .advance_compute_control(&ComputeControlProgress {
                identity,
                expected_revision: claimed.revision,
                state: ComputeControlState::RecoveryRequired,
                stage: ComputeControlStage::DrainingPrevious,
                checkpoint,
                error_code: Some(ERR_BACKEND_ERROR.into()),
                error_message: Some("simulated interrupted drain".into()),
            })
            .await
            .unwrap();
        let runtime = Arc::new(FenceRuntime::default());
        runtime
            .app_target_absent
            .store(target_absent, Ordering::SeqCst);
        // 生产实况: 记录带租约回执, 但集群里的 Lease 对象已不存在 → 探测为 Absent。
        *runtime.compute_inspection.lock().unwrap() = Some(ComputeLeaseInspection::Absent);
        let state = runtime_state(runtime.clone(), store, None).await;
        (state, record, runtime, directory)
    }

    async fn current(state: &AppState, record: &ComputeControlRecord) -> ComputeControlRecord {
        state
            .userapp_store
            .get_compute_control(&record.app_id, &record.operation_id)
            .await
            .unwrap()
            .expect("record persists")
    }

    #[test]
    fn image_roll_restart_predicate_only_matches_roll_checkpoint() {
        let roll = serde_json::json!({"restart_image_roll": true});
        let plain = serde_json::json!({"restart_image_roll": false});
        let mut record = stuck_record_shaped(&roll);
        assert!(is_image_roll_restart(&record));
        record.checkpoint = plain;
        assert!(!is_image_roll_restart(&record));
        record.action = ComputeControlAction::Stop;
        record.checkpoint = roll;
        assert!(!is_image_roll_restart(&record));
    }

    fn stuck_record_shaped(checkpoint: &serde_json::Value) -> ComputeControlRecord {
        ComputeControlRecord {
            created_at: chrono::Utc::now(),
            app_id: "absentroll".into(),
            lifecycle_id: "lifecycle".into(),
            request_id: "request".into(),
            scope: UserAppOperationScope::Prod,
            operation_id: "orphanroll".into(),
            request_fingerprint: "f".repeat(64),
            generation: 1,
            revision: 1,
            action: ComputeControlAction::Restart,
            state: ComputeControlState::RecoveryRequired,
            executor_id: Some("lostexecutor".into()),
            stage: ComputeControlStage::DrainingPrevious.as_str().into(),
            checkpoint: checkpoint.clone(),
            error_code: None,
            error_message: None,
            lease: None,
            interrupted_operations: Vec::new(),
        }
    }

    #[tokio::test]
    async fn absent_roll_target_converges_to_terminal_failed() {
        let (state, record, _runtime, _keep) = stuck_record(
            serde_json::json!({"restart_image_roll": true}),
            true,
            UserAppOperationScope::Prod,
        )
        .await;
        let start = UserAppAdmission {
            app_id: record.app_id.clone(),
            lifecycle_id: Some(record.lifecycle_id.clone()),
            operation_id: "newstart".into(),
            request_id: Some("newstartrequest".into()),
            request_fingerprint: "a".repeat(64),
            kind: UserAppOperationKind::Start,
            command: Some(UserAppControlCommand::Start { traffic: false }),
            metadata: None,
            runtime_policy_on_success: None,
        };
        let Err(UserAppStoreError::OperationInProgress(blocker)) =
            state.userapp_store.admit(&start).await
        else {
            panic!("orphan image roll must block the original prod/start admission");
        };
        assert_eq!(blocker.operation_id, record.operation_id);
        recover_and_execute(&state, &record)
            .await
            .expect("absent-target recovery converges without error");
        let after = current(&state, &record).await;
        assert_eq!(after.state, ComputeControlState::Failed);
        assert_eq!(after.error_code.as_deref(), Some(ERR_RUNTIME_ABSENT));
        assert_eq!(after.checkpoint, record.checkpoint);
        assert!(
            after
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("absent"))
        );
        // 幂等: 终态后再跑一次恢复不报错、不改写。
        recover_and_execute(&state, &after)
            .await
            .expect("terminal record stays inert");
        let again = current(&state, &record).await;
        assert_eq!(again.state, ComputeControlState::Failed);
        assert_eq!(again.revision, after.revision);
        let admitted = state.userapp_store.admit(&start).await.unwrap();
        let UserAppAdmissionOutcome::Accepted(started) = admitted else {
            panic!("terminal orphan must unblock the original prod/start admission");
        };
        assert_eq!(started.operation_id, start.operation_id);
    }

    #[tokio::test]
    async fn dev_scope_builder_roll_ignores_app_absence_probe() {
        // app_compute_absent 探测的是 rcoder-app-{id} 运行时; Dev(builder) 重启即便
        // 带 restart_image_roll 也绝不能据此收敛(Dev 应用常无 prod 运行时 → 恒真)。
        let (state, record, runtime, _keep) = stuck_record(
            serde_json::json!({"restart_image_roll": true}),
            true,
            UserAppOperationScope::Dev,
        )
        .await;
        let _outcome = recover_and_execute(&state, &record).await;
        let after = current(&state, &record).await;
        assert_ne!(
            after.error_code.as_deref(),
            Some(ERR_RUNTIME_ABSENT),
            "Dev(builder) roll must not settle via the prod-runtime absence probe"
        );
        assert_eq!(runtime.app_absence_probes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unproven_absence_or_plain_restart_never_converges_to_absent() {
        // false 只表示缺席未证实；不能据此把 image-roll 收敛为目标缺失。
        let (state, record, _runtime, _keep) = stuck_record(
            serde_json::json!({"restart_image_roll": true}),
            false,
            UserAppOperationScope::Prod,
        )
        .await;
        let _first = recover_and_execute(&state, &record).await;
        let after = current(&state, &record).await;
        assert_ne!(after.error_code.as_deref(), Some(ERR_RUNTIME_ABSENT));
        // 非 image-roll 的 restart 即便目标缺席也不走该收敛(行为保持旧语义)。
        let (state2, record2, runtime2, _keep2) = stuck_record(
            serde_json::json!({"restart_image_roll": false}),
            true,
            UserAppOperationScope::Prod,
        )
        .await;
        let _second = recover_and_execute(&state2, &record2).await;
        let after2 = current(&state2, &record2).await;
        assert_ne!(after2.error_code.as_deref(), Some(ERR_RUNTIME_ABSENT));
        assert_eq!(runtime2.app_absence_probes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn absence_probe_error_retains_original_recovery_until_retry_succeeds() {
        let (state, record, runtime, _keep) = stuck_record(
            serde_json::json!({"restart_image_roll": true}),
            true,
            UserAppOperationScope::Prod,
        )
        .await;
        *runtime.app_absence_error.lock().unwrap() = Some("injected absence read failure".into());
        recover_and_execute(&state, &record).await.unwrap();
        let failed_probe = current(&state, &record).await;
        assert_eq!(failed_probe.state, ComputeControlState::RecoveryRequired);
        assert_eq!(
            failed_probe.error_code.as_deref(),
            Some("COMPUTE_OBSERVATION_FAILED")
        );
        assert!(
            failed_probe
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("injected absence read failure"))
        );
        assert_eq!(failed_probe.checkpoint, record.checkpoint);
        assert_eq!(failed_probe.lease, record.lease);
        *runtime.app_absence_error.lock().unwrap() = None;
        recover_and_execute(&state, &failed_probe).await.unwrap();
        let recovered = current(&state, &record).await;
        assert_eq!(recovered.state, ComputeControlState::Failed);
        assert_eq!(recovered.error_code.as_deref(), Some(ERR_RUNTIME_ABSENT));
    }

    #[tokio::test]
    async fn held_original_lease_prevents_absence_probe_and_terminal_settlement() {
        let (state, record, runtime, _keep) = stuck_record(
            serde_json::json!({"restart_image_roll": true}),
            true,
            UserAppOperationScope::Prod,
        )
        .await;
        *runtime.compute_inspection.lock().unwrap() = Some(ComputeLeaseInspection::Held);
        recover_and_execute(&state, &record).await.unwrap();
        let after = current(&state, &record).await;
        assert_eq!(after.state, ComputeControlState::RecoveryRequired);
        assert_eq!(after.error_code.as_deref(), Some("COMPUTE_LEASE_HELD"));
        assert_eq!(after.checkpoint, record.checkpoint);
        assert_eq!(after.lease, record.lease);
        assert_eq!(runtime.app_absence_probes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stale_absence_settlement_cannot_overwrite_new_recovery_revision() {
        let (state, record, _runtime, _keep) = stuck_record(
            serde_json::json!({"restart_image_roll": true}),
            true,
            UserAppOperationScope::Prod,
        )
        .await;
        let reserved = state
            .userapp_store
            .reserve_compute_drain_recovery(&record)
            .await
            .unwrap();
        let advanced = state
            .userapp_store
            .record_compute_drain_problem(&reserved, "NEW_OBSERVATION", "another replica observed")
            .await
            .unwrap();
        assert!(matches!(
            state
                .userapp_store
                .fail_unrecoverable_compute_control(&reserved, ERR_RUNTIME_ABSENT, "stale absent")
                .await,
            Err(UserAppStoreError::VersionConflict)
        ));
        assert_eq!(current(&state, &record).await, advanced);
        recover_and_execute(&state, &advanced).await.unwrap();
        let recovered = current(&state, &record).await;
        assert_eq!(recovered.state, ComputeControlState::Failed);
        assert_eq!(recovered.error_code.as_deref(), Some(ERR_RUNTIME_ABSENT));
    }
}
