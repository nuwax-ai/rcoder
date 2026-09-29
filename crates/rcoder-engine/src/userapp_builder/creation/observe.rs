use super::*;

pub(crate) async fn observe_created_ready(
    state: &AppState,
    evidence: &shared_types::BuilderCreationEvidence,
) -> Result<Option<ContainerBasicInfo>> {
    let context = &evidence.target.context;
    let before = crate::userapp_builder::adoption::capture_bound_target(state, context).await?;
    validate_completed_resource(evidence, &before, Some(evidence.container.clone()))?;
    let Some(runtime) = state
        .runtime()
        .find_container(&context.app_id, &shared_types::ServiceType::UserappBuilder)
        .await?
    else {
        return Ok(None);
    };
    crate::userapp_builder::validate_builder_identity(&context.app_id, &runtime)?;
    if runtime.container_id != evidence.container.container_id {
        return Err(anyhow!("Builder physical identity changed during recovery"));
    }
    if runtime.status != container_runtime_api::ContainerRuntimeStatus::Running {
        return Ok(None);
    }
    let info = crate::userapp_builder::refreshed_registration(&evidence.container, &runtime)
        .unwrap_or_else(|| evidence.container.clone());
    if !crate::userapp_builder::probe_file_server(&crate::userapp_builder::dev_file_server_addr(
        state, &info,
    )?)
    .await
    {
        return Ok(None);
    }
    let after = crate::userapp_builder::adoption::capture_bound_target(state, context).await?;
    Ok(Some(validate_completed_resource(
        evidence,
        &after,
        Some(info),
    )?))
}

pub(crate) async fn resume_pending(
    state: &AppState,
    pending: &UserAppOperationRecord,
) -> Result<bool> {
    if pending.kind != UserAppOperationKind::EnsureBuilder
        || pending.state != UserAppOperationState::Pending
    {
        return Ok(false);
    }
    let Some(lease) = crate::userapp_builder::lifecycle::try_acquire(&pending.app_id).await else {
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

pub(crate) async fn progress(
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

pub(crate) async fn wait(
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
    let verified = crate::userapp_builder::verify_registration(
        state,
        &accepted.app_id,
        instance,
        &info,
        false,
    )
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
    crate::userapp_builder::register_builder(state, instance, &verified)?;
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
pub(crate) const RECOVERY_REQUIRED_GRACE: Duration = Duration::from_secs(15);

pub(crate) async fn wait_record(
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
                    let human = format!(
                        "Builder operation {} ended as {:?}: {}",
                        operation.operation_id,
                        operation.state,
                        operation
                            .error_message
                            .as_deref()
                            .unwrap_or("Operation result requires inspection")
                    );
                    return Err(
                        if operation.kind == UserAppOperationKind::EnsureBuilder
                            && operation.scope == shared_types::UserAppOperationScope::Dev
                            && operation.checkpoint.get("creation_cancelled")
                                == Some(&serde_json::Value::Bool(true))
                        {
                            // 取消型失败的机器可读根因（chain 可 downcast）：
                            // 只说明本次创建已收束，控制操作仍需按持久状态等待。
                            anyhow::Error::new(crate::userapp_builder::BuilderEnsureSuperseded {
                                operation_id: operation.operation_id.clone(),
                            })
                            .context(human)
                        } else {
                            anyhow::Error::msg(human)
                        },
                    );
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
pub(crate) async fn reconcile_completed(
    state: &AppState,
    snapshot: &UserAppOperationRecord,
) -> Result<bool> {
    if snapshot.kind != UserAppOperationKind::EnsureBuilder
        || !shared_types::userapp_operation_has_final_evidence(snapshot)
    {
        return Ok(false);
    }
    let Some(_local) = crate::userapp_builder::lifecycle::try_acquire(&snapshot.app_id).await
    else {
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
pub(crate) fn validate_completed_resource(
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
