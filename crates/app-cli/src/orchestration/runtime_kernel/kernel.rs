use super::*;

/// 受理判定结果。
#[derive(Debug)]
pub(crate) enum AdmissionOutcome {
    /// 幂等重放：返回既有记录（不重复执行）。
    Replayed(RuntimeOperationView),
    /// 新受理（已持久化 Accepted，等待执行）。
    Accepted(RuntimeOperationView),
}

/// 提交屏障裁决（B03）。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum CommitBarrierOutcome {
    /// 屏障通过并已收束 Succeeded。
    Committed,
    /// 已被取消（观察到的取消意图；**未写终态**）——调用方必须先停服并
    /// 确认清理，再经 finish 收束 Cancelled/RecoveryRequired（V02）。
    Cancelled,
    /// Stop 已受理（revision 推进）：调用方必须停服并让 Stop 执行，
    /// 本操作保持非 Succeeded（由调用方停服后收束 Cancelled）。
    Superseded,
    /// 操作已不是 active 执行者（并发收束/替换）——无操作。
    NotActive,
}

/// 受理拒绝（API 层映射 HTTP 409/400 信封）。
#[derive(Debug)]
pub(crate) struct AdmissionRejection {
    pub code: &'static str,
    pub message: String,
    /// 进行中操作 ID（ERR_OPERATION_IN_PROGRESS 携带）。
    pub active_operation_id: Option<String>,
}

/// 运行操作内核：受理/查询/事件/取消的协调面（执行仍归 server 主循环）。
pub(crate) struct RuntimeKernel {
    pub(super) store: RuntimeStore,
    identity: RuntimeIdentityView,
    /// 受理短锁（不跨执行 await；执行互斥由 active_operation 单槽承担）。
    pub(super) admission: Mutex<AdmissionState>,
    /// 执行派发回调（server 主循环注入；制品→部署通道，源码/停止→编排通道）。
    dispatch: Box<dyn Fn(DispatchAction) + Send + Sync>,
    /// 已请求取消的操作墓碑（R03）：受理后、执行完成前的取消标记。
    /// 执行侧在副作用边界检查（派发执行前/编排完成提交前）。
    cancelled: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Serialize sequence allocation and append across every event producer.
    event_write: std::sync::Mutex<()>,
}

/// 执行派发动作（server 主循环解释；内核不直接触碰业务运行态）。
#[derive(Debug, Clone)]
pub(crate) enum DispatchAction {
    /// 制品部署（复用既有部署准备/激活/编排全链）。
    DeployArtifact {
        operation_id: String,
        url: String,
        sha256: Option<String>,
        pg: Option<shared_types::StartPgCredential>,
    },
    /// 登记的本地构建制品部署（R03：平台 staging 后 owner 受理激活——
    /// 制品 zip 在共享卷 `builds/` 目录，不经网络下载）。
    DeployLocalArtifact {
        operation_id: String,
        artifact_id: String,
        sha256: Option<String>,
        pg: Option<shared_types::StartPgCredential>,
    },
    /// 源码编排（workspace 当前内容 + release lock；start/restart source）。
    /// R08：dev_profile 随操作传递（Source profile = dev 语义）——编排的
    /// 生效命令选择不再依赖 serve 进程 env 猜测；pg 为每操作运行配置
    /// （凭据注入服务 env，不落盘——持久化副本在 store_operation 前脱敏）。
    OrchestrateSource {
        operation_id: String,
        dev_profile: bool,
        pg: Option<shared_types::StartPgCredential>,
    },
    /// 停止业务服务（保持管理面可用）。
    StopBusiness { operation_id: String },
}

#[derive(Default)]
pub(super) struct AdmissionState {
    /// 当前执行者（None = 空闲；单槽即单 worker 串行）。V03：只有**执行型**
    /// 操作（start/restart/deploy）占据此槽——Stop 是待执行意图，绝不抢走
    /// 在执行者的身份（旧 A 的提交屏障因此不再被误判 NotActive）。
    pub(super) active_operation_id: Option<String>,
    /// 已受理待执行的 Stop（V03）：持有者等待 server 主循环停服后按自身 ID
    /// 收束。新启动保留最新意图，等待 Stop 完成；重复 Stop 重放原请求。
    pending_stop: Option<String>,
    /// R02"最后受理生效"：active 执行期间受理的 Start/Restart 排队槽
    /// （单槽，最新覆盖旧的——被覆盖者收束 Cancelled/ERR_SUPERSEDED）。
    /// active 确定收束且无 pending_stop 时派发；Stop 受理时清空
    /// （停止意图胜过排队启动）。
    pub(super) pending_restart: Option<String>,
    /// Original execution inputs never reconstructed from redacted disk records.
    queued_input: Option<StoredOperation>,
    /// 恢复保护中（上次结果未知）。V03：由**结果未知**决定，不依赖 active
    /// 恰好匹配——任何操作的 RecoveryRequired 终态都会挂起保护。
    pub(super) recovery_protection: bool,
    /// Only records discovered before this owner's execution began may be
    /// settled by admission recovery. An admission mutex is not a worker lock.
    recovered_operations: std::collections::HashSet<String>,
    /// RV01：上一个已结束业务会话遗留的待执行 Stop（意图屏障）。清理由
    /// 会话终末的后台任务确认；下一次会话启动（prepare_relaunch）在围栏
    /// 清空后按幂等语义收束为 Succeeded。
    pub(super) interrupted_stop: Option<String>,
    /// Operations actually consumed by the current execution driver. Admissions
    /// and dispatch alone do not associate a request with that driver's outcome.
    consumed_operations: std::collections::HashSet<String>,
    /// Consumed operations retained for recovery after their driver ended.
    /// Unconsumed admissions keep their original slot and execution input.
    pub(super) interrupted_operations: Vec<String>,
    /// Process-local foreground lifetime bookkeeping. Ordinary serve failures
    /// never set the exit latch; a new owner starts with it cleared.
    latest_admitted_operation_id: Option<String>,
    foreground_exiting_operation: Option<String>,
}

/// RV01：当前持有执行权或待交接收束的操作 ID 集——恢复扫描与保护计算
/// 必须跳过它们（活跃受理的派发状态由通道消费路径核验，不是未知结果）。
fn live_execution_ids(guard: &AdmissionState) -> std::collections::HashSet<String> {
    let mut live = std::collections::HashSet::new();
    if let Some(id) = &guard.active_operation_id {
        live.insert(id.clone());
    }
    if let Some(id) = &guard.pending_stop {
        live.insert(id.clone());
    }
    if let Some(id) = &guard.pending_restart {
        live.insert(id.clone());
    }
    if let Some(id) = &guard.interrupted_stop {
        live.insert(id.clone());
    }
    for id in &guard.interrupted_operations {
        live.insert(id.clone());
    }
    live
}

impl RuntimeKernel {
    pub(crate) fn new(
        store: RuntimeStore,
        identity: RuntimeIdentityView,
        dispatch: Box<dyn Fn(DispatchAction) + Send + Sync>,
    ) -> Self {
        Self {
            store,
            identity,
            admission: Mutex::new(AdmissionState::default()),
            dispatch,
            cancelled: std::sync::Mutex::new(std::collections::HashSet::new()),
            event_write: std::sync::Mutex::new(()),
        }
    }

    pub(crate) fn identity(&self) -> &RuntimeIdentityView {
        &self.identity
    }

    pub(crate) fn store(&self) -> &RuntimeStore {
        &self.store
    }

    /// Capture consumption at the driver's side-effect boundary. A late signal
    /// for a released slot or a terminal record must not execute again.
    pub(crate) async fn mark_execution_consumed(&self, operation_id: &str) -> Result<bool> {
        let mut guard = self.admission.lock().await;
        if guard.active_operation_id.as_deref() != Some(operation_id)
            && guard.pending_stop.as_deref() != Some(operation_id)
        {
            return Ok(false);
        }
        let stored = self
            .store
            .load_operation(operation_id)?
            .context("consumed operation record is missing")?;
        if stored.view.state.is_terminal()
            || stored.view.state == RuntimeOperationState::RecoveryRequired
        {
            return Ok(false);
        }
        anyhow::ensure!(
            stored.view.operation_id == operation_id
                && stored.request.operation_id == operation_id
                && stored.view.runtime_instance_id == self.identity.runtime_instance_id
                && stored.request.workspace_id == self.identity.workspace_id,
            "consumed operation identity changed"
        );
        guard.consumed_operations.insert(operation_id.to_owned());
        Ok(true)
    }

    /// End the driver's captured execution set. Reaching server_loop is not
    /// proof that it consumed every admission currently occupying a slot.
    pub(crate) async fn note_execution_session_ended(&self, reached_driver: bool) -> Result<()> {
        let mut guard = self.admission.lock().await;
        if !reached_driver && guard.consumed_operations.is_empty() {
            return Ok(());
        }
        let consumed = std::mem::take(&mut guard.consumed_operations);
        for id in consumed {
            if guard.active_operation_id.as_deref() == Some(id.as_str()) {
                guard.active_operation_id = None;
            }
            if guard.pending_stop.as_deref() == Some(id.as_str()) {
                guard.pending_stop = None;
                guard.interrupted_stop = Some(id.clone());
            }
            if !guard.interrupted_operations.contains(&id) {
                guard.interrupted_operations.push(id);
            }
        }
        Ok(())
    }

    /// Startup may preserve native Stopped intent only while no accepted request
    /// owns the execution slots. Check and publish under the admission lock so
    /// an explicit fresh Start cannot be overwritten after a separate idle read.
    pub(crate) async fn ensure_stopped_if_idle(&self) -> Result<bool> {
        let guard = self.admission.lock().await;
        if guard.active_operation_id.is_some()
            || guard.pending_stop.is_some()
            || guard.pending_restart.is_some()
            || guard.queued_input.is_some()
            || !guard.consumed_operations.is_empty()
            || !guard.interrupted_operations.is_empty()
            || guard.interrupted_stop.is_some()
        {
            return Ok(false);
        }
        let (desired, revision) = self.store.load_desired()?;
        if desired == DesiredState::Stopped {
            return Ok(false);
        }
        let next_revision = revision.checked_add(1).context("revision overflow")?;
        self.store
            .store_desired(DesiredState::Stopped, next_revision)?;
        Ok(true)
    }

    /// Called after physical cleanup and startup recovery are confirmed. Resume
    /// an unconsumed queued request using its retained original input; do not
    /// dispatch while a control barrier or uncertain prior execution remains.
    pub(crate) async fn dispatch_pending_after_quiescence(&self) -> Result<bool> {
        let mut guard = self.admission.lock().await;
        if guard.active_operation_id.is_some()
            || guard.pending_stop.is_some()
            || guard.interrupted_stop.is_some()
            || !guard.interrupted_operations.is_empty()
            || guard.recovery_protection
            || guard.pending_restart.is_none()
        {
            return Ok(false);
        }
        self.promote_queued(&mut guard)?;
        Ok(true)
    }

    /// RV03/RV01：是否存在已受理待消费的执行操作（驱动前失败的会话驻留
    /// 时据此重新触发会话重建——单次 relaunch 通知可能被一次失败重试
    /// 消费掉，预算再度耗尽后没有任何唤醒者，用户操作将无限滞留）。
    pub(crate) async fn has_live_admission(&self) -> bool {
        let guard = self.admission.lock().await;
        guard.active_operation_id.is_some() || guard.pending_restart.is_some()
    }

    /// 会话交接收束（RV01 重整）：只收束 [`RuntimeKernel::note_execution_session_ended`]
    /// 记录的、属于**已结束会话**的操作集，绝不清扫当前槽位——会话结束后
    /// 新受理的 Start/Stop 正占据这些槽位等待下一个会话的执行循环消费。
    /// 挂起 Stop 屏障按 Succeeded 收束：下一个会话的启动以围栏清空为前置
    /// （上代清理已确认），物理停止由会话终末清理完成，幂等且如实。
    /// 其余被中断操作保持 RecoveryRequired 语义，不伪造终态。
    pub(crate) async fn prepare_relaunch(&self) -> Result<()> {
        let mut guard = self.admission.lock().await;
        let stop = guard.interrupted_stop.take();
        let mut stale = std::mem::take(&mut guard.interrupted_operations);
        if let Some(stop) = stop.as_ref()
            && !stale.contains(stop)
        {
            stale.push(stop.clone());
        }
        if stale.is_empty() {
            return Ok(());
        }
        let mut unsettled = Vec::new();
        let mut first_error: Option<anyhow::Error> = None;
        for id in &stale {
            let is_pending_stop = stop.as_deref() == Some(id.as_str());
            let persisted = self.store.load_operation(id);
            match persisted {
                Ok(Some(operation)) => {
                    if !operation.view.state.is_terminal() {
                        // 会话结束即业务已被 business_ended 协作停止并清理
                        //（spec §2.3 幂等停止）：挂起的 Stop 屏障按 Succeeded
                        // 收束——不是伪造，物理停止由会话终末清理完成。
                        let terminal = if is_pending_stop {
                            RuntimeOperationState::Succeeded
                        } else {
                            RuntimeOperationState::RecoveryRequired
                        };
                        let detail = if is_pending_stop {
                            None
                        } else {
                            Some((
                                shared_types::ERR_RECOVERY_REQUIRED.to_string(),
                                "business session ended before the operation completed".to_string(),
                            ))
                        };
                        if let Err(error) = self.write_terminal(id, terminal, detail, None) {
                            unsettled.push(id.clone());
                            first_error.get_or_insert(error);
                        }
                    }
                }
                Ok(None) => {
                    tracing::warn!(
                        operation_id = %id,
                        "interrupted session slot has no persisted operation; clearing"
                    );
                }
                Err(error) => {
                    // 持久化读取/写入失败：该操作保持待收束，下一次会话重试。
                    unsettled.push(id.clone());
                    first_error.get_or_insert(error);
                }
            }
        }
        if !unsettled.is_empty() {
            guard.interrupted_operations = unsettled;
            if let Some(stop) = stop.filter(|id| guard.interrupted_operations.contains(id)) {
                guard.interrupted_stop = Some(stop);
            }
            guard.recovery_protection = true;
            let error =
                first_error.unwrap_or_else(|| anyhow::anyhow!("operations remain unsettled"));
            return Err(anyhow::anyhow!(
                "settle interrupted session operations: {error:#}"
            ));
        }
        self.refresh_recovery_protection(&mut guard);
        Ok(())
    }

    pub(crate) async fn recover(&self) -> Result<Vec<String>> {
        let mut guard = self.admission.lock().await;
        // RV01：active-is-none 硬前置移除。槽位此时只可能持有**会话结束后
        // 新受理**的操作（上一会话的占据者已由 note_execution_session_ended
        // 移入待交接集），它们派发进通道、等待本会话的执行循环消费——不是
        // "本 owner 正在执行"。恢复扫描跳过这些活跃受理（见 live 集），
        // 不把已知派发状态的受理当未知结果销毁。
        let live = live_execution_ids(&guard);
        self.store.repair_desired_after_quiescence()?;
        let mut scan = self.store.recover_unfinished_operations(&live)?;
        guard
            .recovered_operations
            .extend(scan.recovered.iter().cloned());
        // 恢复保护：在途操作待精确/兜底收敛，或隔离失败（存储层故障）
        // 时拒绝新写（spec §3.3 崩溃注入语义；R04 把 fail-closed 从注释
        // 变成实际行为）。待交接集合尚未收束（prepare 失败重试窗口）
        // 同样保持保护。
        guard.recovery_protection = !scan.recovered.is_empty()
            || !scan.blocked.is_empty()
            || !guard.interrupted_operations.is_empty()
            || guard.interrupted_stop.is_some();
        for id in &scan.recovered {
            let recorded = self
                .store
                .load_operation(id)
                .and_then(|operation| operation.context("recovered operation disappeared"))
                .and_then(|operation| self.store.cancellation_recorded(&operation));
            let requested = match recorded {
                Ok(requested) => requested,
                Err(error) => {
                    tracing::error!(operation_id = %id, %error, "Cancellation recovery requires reconciliation");
                    scan.blocked
                        .push(self.store.cancellation_path(id).display().to_string());
                    guard.recovery_protection = true;
                    true
                }
            };
            if requested {
                self.cancelled
                    .lock()
                    .map_err(|_| anyhow::anyhow!("cancel registry lock poisoned"))?
                    .insert(id.clone());
            }
        }
        if !scan.quarantined.is_empty() {
            tracing::warn!(
                count = scan.quarantined.len(),
                paths = ?scan.quarantined,
                "runtime state: damaged operation records were quarantined; originals are preserved"
            );
        }
        if !scan.blocked.is_empty() {
            tracing::error!(
                count = scan.blocked.len(),
                paths = ?scan.blocked,
                "runtime state records could not be read or quarantined; writes are blocked until storage recovers"
            );
        }
        Ok(scan.recovered)
    }

    /// 启动序列末尾兜底收敛：精确收敛路径（journal receipt、持久化 Stop
    /// 意图）之后仍非终态的操作在此沉降为 Failed。当前容器已取得管理权，
    /// 本地旧进程已收束；其他容器的退役由平台负责。记录仅表示结果未提交——
    /// 不伪造成功，也不留 RecoveryRequired 死锁（用户/agent 的 start、
    /// 重复编译构建、停止回收后再启动都必须能继续执行）。
    pub(crate) async fn settle_unresolved_recoveries(&self) -> Result<Vec<String>> {
        let mut guard = self.admission.lock().await;
        self.settle_unresolved_recoveries_locked(&mut guard)
    }

    /// Retry startup bookkeeping after a transient storage failure. Never settle
    /// the current owner's active or queued work, even while recovery is latched.
    pub(super) fn settle_unresolved_recoveries_locked(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, AdmissionState>,
    ) -> Result<Vec<String>> {
        let mut settled = Vec::new();
        for entry in
            std::fs::read_dir(self.store.root.join("operations")).context("scan operations dir")?
        {
            let entry = entry.context("read operations dir entry")?;
            let path = entry.path();
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    anyhow::bail!("read operation record {}: {error}", path.display())
                }
            };
            let mut operation: StoredOperation = match serde_json::from_slice(&bytes) {
                Ok(operation) => operation,
                Err(error) => {
                    anyhow::bail!("decode operation record {}: {error}", path.display())
                }
            };
            let id = &operation.view.operation_id;
            if operation.view.state.is_terminal()
                || !guard.recovered_operations.contains(id)
                || guard.active_operation_id.as_ref() == Some(id)
                || guard.pending_stop.as_ref() == Some(id)
                || guard.pending_restart.as_ref() == Some(id)
            {
                continue;
            }
            let requested = match self.store.cancellation_recorded(&operation) {
                Ok(requested) => requested,
                Err(error) => {
                    tracing::warn!(operation_id = %id, %error, "interrupted cancellation record retained for diagnosis");
                    true
                }
            };
            let id = operation.view.operation_id.clone();
            // recovery v3：Stop 幂等收敛——统一 owner 的 API 在首个业务
            // 会话启动前即可受理 Stop（R3 早期开放），该操作可能在无会话
            // 消费它的窗口内被本路径按"中断"沉降。停止已确认不存在的业务
            // 幂等成功（spec §2.3），且会话启动的 quiescence 已实际收束
            // 业务进程——Succeeded 是如实结果，不是伪造。非 Stop 保持
            // Failed（不发明成功）。
            if operation.view.kind == RuntimeOperationKind::Stop {
                self.write_terminal(&id, RuntimeOperationState::Succeeded, None, None)?;
            } else {
                self.store.settle_interrupted_operation(&mut operation)?;
                if requested && let Ok(mut set) = self.cancelled.lock() {
                    set.insert(id.clone());
                }
                self.emit_terminal_record(&operation);
            }
            guard.recovered_operations.remove(&id);
            settled.push(id);
        }
        if !settled.is_empty() {
            tracing::warn!(
                count = settled.len(),
                operations = ?settled,
                "startup settled interrupted operations as Failed (previous owner exit)"
            );
        }
        self.refresh_recovery_protection(guard);
        Ok(settled)
    }

    pub(crate) async fn status(&self) -> Result<RuntimeStatusView> {
        let (_desired, revision) = self.store.load_desired()?;
        let guard = self.admission.lock().await;
        Ok(RuntimeStatusView {
            desired: _desired,
            observed: shared_types::ObservedHealth::Unknown,
            active_target: None,
            revision,
            active_operation_id: guard.active_operation_id.clone(),
            recovery_protection: guard.recovery_protection,
            runtime_instance_id: self.identity.runtime_instance_id.clone(),
        })
    }

    /// Startup-only reconciliation after the previous owner was quiesced.
    /// Active/StartupFailed confirms the artifact, while the caller verifies
    /// current process quiescence. Application migration history is diagnostic.
    /// An uncommitted terminal result
    /// becomes Failed (or Cancelled), never an invented success.
    pub(crate) async fn reconcile_quiesced_operation(
        &self,
        receipt: &crate::server::journal::Receipt,
    ) -> Result<bool> {
        use crate::server::journal::Boundary;
        let preparation_only = receipt.boundary == Boundary::Preparing
            && matches!(
                receipt.operation.phase,
                shared_types::AppCliDeployPhase::Deploying
                    | shared_types::AppCliDeployPhase::Failed
            )
            && matches!(
                receipt.operation.deploy_stage,
                shared_types::app_cli_deploy::AppDeploymentStage::Pending
                    | shared_types::app_cli_deploy::AppDeploymentStage::Failed
            );
        let confirmed_boundary = matches!(
            (&receipt.boundary, receipt.operation.phase),
            (
                Boundary::StartupFailed,
                shared_types::AppCliDeployPhase::Failed
            ) | (Boundary::Active, shared_types::AppCliDeployPhase::Running)
        ) && receipt.operation.persisted
            && receipt.operation.deploy_stage
                == shared_types::app_cli_deploy::AppDeploymentStage::Succeeded;
        if !(preparation_only || confirmed_boundary)
            || receipt.generation != self.identity.deployment_generation_id
            || receipt.operation.recovery.is_some()
        {
            return Ok(false);
        }
        let Some(id) = receipt.request.runtime_operation_id.as_ref() else {
            return Ok(false);
        };
        anyhow::ensure!(
            id == &receipt.operation.operation_id,
            "startup failure journal operation mismatch"
        );
        let mut guard = self.admission.lock().await;
        anyhow::ensure!(
            guard.active_operation_id.is_none()
                && guard.pending_stop.is_none()
                && guard.pending_restart.is_none(),
            "runtime execution is active during recovery"
        );
        let Some(mut operation) = self.store.load_operation(id)? else {
            return Ok(false);
        };
        if operation.view.state != RuntimeOperationState::RecoveryRequired {
            return Ok(false);
        }
        anyhow::ensure!(
            operation.view.kind != RuntimeOperationKind::Stop
                && operation.request.workspace_id == self.identity.workspace_id
                && operation.request.operation_id == *id
                && operation.view.request_digest
                    == runtime_request_digest(&operation.request).map_err(anyhow::Error::msg)?,
            "startup failure runtime identity mismatch"
        );
        let cancelled = self.store.cancellation_recorded(&operation)?;
        operation.view.state = if cancelled {
            RuntimeOperationState::Cancelled
        } else {
            RuntimeOperationState::Failed
        };
        operation.view.error_code = Some(
            if cancelled {
                "ERR_CANCELLED"
            } else if receipt.boundary == Boundary::Active || preparation_only {
                "ERR_OPERATION_INTERRUPTED"
            } else {
                "ERR_STARTUP_FAILED"
            }
            .into(),
        );
        operation.view.error_message = if preparation_only {
            receipt.operation.error.clone().or_else(|| {
                Some("Previous owner stopped during preparation before directory activation".into())
            })
        } else if receipt.boundary == Boundary::Active {
            Some(
                "Previous owner was stopped before the operation terminal result was committed"
                    .into(),
            )
        } else {
            receipt.operation.error.clone()
        };
        operation.view.failure_detail = Some(RuntimeFailureDetail {
            stage: "startup_reconciled".into(),
            exit_code: None,
            stderr_tail: None,
            cleanup_confirmed: true,
        });
        self.store.store_operation(&operation)?;
        self.emit_terminal_record(&operation);
        self.refresh_recovery_protection(&mut guard);
        Ok(true)
    }

    pub(super) fn refresh_recovery_protection(&self, guard: &mut AdmissionState) {
        let live = live_execution_ids(guard);
        let unresolved = (|| -> Result<bool> {
            for entry in std::fs::read_dir(self.store.root.join("operations"))? {
                let bytes = std::fs::read(entry?.path())?;
                let other: StoredOperation = serde_json::from_slice(&bytes)?;
                if live.contains(&other.view.operation_id) {
                    // RV01：活跃受理（已派发待消费/待交接收束）不是未知
                    // 结果——不因其 Accepted 而挂起恢复保护。
                    continue;
                }
                if !other.view.state.is_terminal() {
                    return Ok(true);
                }
            }
            Ok(false)
        })();
        guard.recovery_protection = match unresolved {
            Ok(unresolved) => unresolved,
            Err(error) => {
                tracing::error!(%error, "Remaining runtime recovery records could not be verified");
                true
            }
        };
    }

    /// RV03：仍非终态的 Stop 操作（无驱动者时期望有执行者收尾的集合）。
    /// 供降级驻留 owner 的 Stop 看门狗按"已确认无执行"幂等收束。
    pub(crate) fn outstanding_stop_operations(&self) -> Result<Vec<String>> {
        let mut outstanding = Vec::new();
        for entry in
            std::fs::read_dir(self.store.root.join("operations")).context("scan operations dir")?
        {
            let entry = entry.context("read operations dir entry")?;
            let bytes = match std::fs::read(entry.path()) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(anyhow::Error::new(error)
                        .context(format!("read operation record {}", entry.path().display())));
                }
            };
            let operation: StoredOperation = match serde_json::from_slice(&bytes) {
                Ok(operation) => operation,
                Err(error) => {
                    return Err(anyhow::Error::new(error).context(format!(
                        "decode operation record {}",
                        entry.path().display()
                    )));
                }
            };
            if operation.view.kind == RuntimeOperationKind::Stop
                && !operation.view.state.is_terminal()
            {
                outstanding.push(operation.view.operation_id);
            }
        }
        Ok(outstanding)
    }

    /// Called only during startup after all previous business processes have
    /// been confirmed stopped. Complete the exact persisted stop intent; do not
    /// execute a new stop or modify desired/revision.
    pub(crate) async fn reconcile_quiesced_stop(&self) -> Result<bool> {
        let mut guard = self.admission.lock().await;
        anyhow::ensure!(
            guard.active_operation_id.is_none()
                && guard.pending_stop.is_none()
                && guard.pending_restart.is_none(),
            "runtime execution is active during stop recovery"
        );
        let (desired, revision) = self.store.load_desired()?;
        if desired != DesiredState::Stopped {
            return Ok(false);
        }
        let mut candidate = None;
        for entry in std::fs::read_dir(self.store.root.join("operations"))? {
            let operation: StoredOperation =
                serde_json::from_slice(&std::fs::read(entry?.path())?)?;
            if operation.view.state != RuntimeOperationState::RecoveryRequired
                || operation.view.kind != RuntimeOperationKind::Stop
                || operation.view.revision != revision
            {
                continue;
            }
            anyhow::ensure!(
                operation.request.kind == RuntimeOperationKind::Stop
                    && operation.request.workspace_id == self.identity.workspace_id
                    && operation.view.operation_id == operation.request.operation_id
                    && operation.request.expected_revision.checked_add(1) == Some(revision)
                    && operation.view.request_digest
                        == runtime_request_digest(&operation.request)
                            .map_err(anyhow::Error::msg)?,
                "persisted stop identity mismatch"
            );
            anyhow::ensure!(
                candidate.is_none(),
                "multiple operations claim the same stop revision"
            );
            candidate = Some(operation);
        }
        let Some(mut operation) = candidate else {
            return Ok(false);
        };
        operation.view.state = RuntimeOperationState::Succeeded;
        operation.view.error_code = None;
        operation.view.error_message = None;
        operation.view.failure_detail = None;
        self.store.store_operation(&operation)?;
        self.emit_terminal_record(&operation);
        self.refresh_recovery_protection(&mut guard);
        Ok(true)
    }

    /// 受理（spec §3.3）：短锁内完成全部检查与持久化，执行派发在锁外。
    pub(crate) async fn admit(
        &self,
        request: RuntimeOperationRequest,
    ) -> std::result::Result<AdmissionOutcome, AdmissionRejection> {
        self.admit_with_owner_hold(request, false).await
    }

    pub(crate) async fn admit_with_owner_hold(
        &self,
        request: RuntimeOperationRequest,
        owner_recovery_hold: bool,
    ) -> std::result::Result<AdmissionOutcome, AdmissionRejection> {
        self.admit_with_control_hold(request, owner_recovery_hold, None)
            .await
    }

    pub(crate) async fn admit_with_control_hold(
        &self,
        request: RuntimeOperationRequest,
        owner_recovery_hold: bool,
        native_control: Option<(String, String)>,
    ) -> std::result::Result<AdmissionOutcome, AdmissionRejection> {
        if let Err(error) = validate_runtime_operation_request(&request) {
            return Err(AdmissionRejection {
                code: "ERR_VALIDATION",
                message: error,
                active_operation_id: None,
            });
        }
        let digest = runtime_request_digest(&request).map_err(|error| AdmissionRejection {
            code: "ERR_VALIDATION",
            message: error,
            active_operation_id: None,
        })?;
        // R03：kind×profile 组合前置校验——未实现组合在**任何持久化/占位
        // 之前**结构化拒绝（不留半受理状态）。已实现：Deploy+Artifact(Url
        // 或 ArtifactId——R03 owner 侧激活的登记本地制品)、Start/Restart+
        // Source、Stop（任意 profile）。
        match (&request.kind, &request.profile) {
            (
                RuntimeOperationKind::Deploy,
                shared_types::RunProfileInput::Artifact { artifact: _ },
            )
            | (
                RuntimeOperationKind::Start | RuntimeOperationKind::Restart,
                shared_types::RunProfileInput::Source { .. },
            )
            | (RuntimeOperationKind::Stop, _) => {}
            (kind, profile) => {
                return Err(AdmissionRejection {
                    code: shared_types::ERR_PROTOCOL_UNSUPPORTED,
                    message: format!(
                        "operation kind {kind} with profile {profile:?} is not implemented \
                         in this build; supported: deploy+artifact(url or artifact_id), start/restart+source, stop"
                    ),
                    active_operation_id: None,
                });
            }
        }
        // 身份核验先于一切重放判断：旧实例请求不修改新实例（spec §3.1）。
        if request.expected_runtime_instance_id != self.identity.runtime_instance_id {
            return Err(AdmissionRejection {
                code: ERR_RUNTIME_INSTANCE_MISMATCH,
                message: format!(
                    "expected runtime instance {} does not match current {}",
                    request.expected_runtime_instance_id, self.identity.runtime_instance_id
                ),
                active_operation_id: None,
            });
        }
        if request.workspace_id != self.identity.workspace_id {
            return Err(AdmissionRejection {
                code: ERR_WORKSPACE_MISMATCH,
                message: format!(
                    "workspace {} does not match owner binding {}",
                    request.workspace_id, self.identity.workspace_id
                ),
                active_operation_id: None,
            });
        }
        let mut guard = self.admission.lock().await;
        // 幂等重放先于 busy 拒绝（spec：先查重放再拒绝 busy）。
        if let Some(existing) =
            self.store
                .load_operation(&request.operation_id)
                .map_err(|error| AdmissionRejection {
                    code: "ERR_BACKEND_ERROR",
                    message: format!("read operation history: {error:#}"),
                    active_operation_id: None,
                })?
        {
            if existing.view.request_digest != digest {
                return Err(AdmissionRejection {
                    code: ERR_OPERATION_ID_CONFLICT,
                    message: format!(
                        "operation {} was accepted with a different request",
                        request.operation_id
                    ),
                    active_operation_id: None,
                });
            }
            return Ok(AdmissionOutcome::Replayed(existing.view));
        }
        if request.kind != RuntimeOperationKind::Stop
            && let Some(operation_id) = guard.foreground_exiting_operation.as_ref()
        {
            return Err(AdmissionRejection {
                code: ERR_OPERATION_IN_PROGRESS,
                message: format!(
                    "foreground owner is exiting after operation {operation_id}; retry after it exits"
                ),
                active_operation_id: Some(operation_id.clone()),
            });
        }
        // Same-request replay precedes the native control barrier. A different
        // execution request is rejected before durable admission; Stop remains
        // available to converge the current physical execution.
        if request.kind != RuntimeOperationKind::Stop
            && let Some((operation_id, stage)) = native_control
        {
            return Err(AdmissionRejection {
                code: ERR_OPERATION_IN_PROGRESS,
                message: format!(
                    "native control operation {operation_id} is in progress at {stage}; \
                     retry after it finishes"
                ),
                active_operation_id: Some(operation_id),
            });
        }
        if owner_recovery_hold && request.kind != RuntimeOperationKind::Stop {
            return Err(AdmissionRejection {
                code: ERR_RECOVERY_REQUIRED,
                message: "owner recovery must be resolved before starting business".into(),
                active_operation_id: None,
            });
        }
        if guard.recovery_protection && request.kind != RuntimeOperationKind::Stop {
            // Retry only startup records. Current work must finish or be stopped
            // by the independent supervisor; admission does not prove its exit.
            let settled = self
                .settle_unresolved_recoveries_locked(&mut guard)
                .map_err(|error| AdmissionRejection {
                    code: ERR_RECOVERY_REQUIRED,
                    message: format!("automatic recovery failed: {error:#}"),
                    active_operation_id: None,
                })?;
            if !settled.is_empty() {
                tracing::warn!(
                    count = settled.len(),
                    operations = ?settled,
                    "admission settled interrupted operations before accepting a new request"
                );
            }
            if guard.recovery_protection {
                return Err(AdmissionRejection {
                    code: ERR_RECOVERY_REQUIRED,
                    message: "a previous operation has an unconfirmed result; recovery is required"
                        .into(),
                    active_operation_id: None,
                });
            }
        }
        let (_desired, revision) =
            self.store
                .load_desired()
                .map_err(|error| AdmissionRejection {
                    code: "ERR_BACKEND_ERROR",
                    message: format!("read desired state: {error:#}"),
                    active_operation_id: None,
                })?;
        // Stop 屏障例外（spec §3.3）：active 执行期间仍可受理持久化停止意图
        // ——Stop 只占 pending 槽，绝不抢走执行者身份（V03）。
        // 第二个 Stop 应复用原请求查询；新启动可保留最新意图。
        let is_stop = request.kind == RuntimeOperationKind::Stop;
        if is_stop && let Some(pending) = guard.pending_stop.clone() {
            return Err(AdmissionRejection {
                code: ERR_OPERATION_IN_PROGRESS,
                message: "another runtime operation is in progress".into(),
                active_operation_id: Some(pending),
            });
        }
        // A new start accepted during Stop occupies the single latest-intent
        // slot. The execution loop dispatches it only after Stop actually ends.
        if request.expected_revision != revision {
            // 所有 kind 的 revision 校验统一执行（旧实例的 stop 不复活/不重复推进）。
            return Err(AdmissionRejection {
                code: ERR_REVISION_MISMATCH,
                message: format!(
                    "expected revision {} but current revision is {}",
                    request.expected_revision, revision
                ),
                active_operation_id: None,
            });
        }
        let next_revision = if is_stop {
            revision.checked_add(1).ok_or_else(|| AdmissionRejection {
                code: "ERR_BACKEND_ERROR",
                message: "revision overflow".into(),
                active_operation_id: None,
            })?
        } else {
            revision
        };
        // R5（recovery v3）：物理 Stop/Restart 执行期间，不同的
        // Start/Restart 在持久化新操作前返回 Busy（附当前操作身份），
        // 不进入内部排队；普通构建/部署的自动接替规则保持不变。
        // 同请求重试在更早的重放判定中返回已记录进度，不受此分支影响。
        if !is_stop && (guard.pending_stop.is_some() || guard.active_operation_id.is_some()) {
            let busy_control = guard.pending_stop.is_some() || {
                // RV09：占槽记录读取失败必须如实拒绝（不能 .ok 折叠成
                // "非控制操作"绕过 Busy 判定后静默排队）。
                let active = match guard
                    .active_operation_id
                    .as_deref()
                    .map(|id| self.store.load_operation(id))
                {
                    Some(Ok(operation)) => operation,
                    Some(Err(error)) => {
                        return Err(AdmissionRejection {
                            code: "ERR_BACKEND_ERROR",
                            message: format!("read active operation record: {error:#}"),
                            active_operation_id: None,
                        });
                    }
                    None => None,
                };
                active.is_some_and(|stored| {
                    matches!(
                        stored.view.kind,
                        RuntimeOperationKind::Stop | RuntimeOperationKind::Restart
                    ) && !stored.view.state.is_terminal()
                })
            };
            if busy_control {
                // 零副作用拒绝：不持久化受理、不改意图/revision。
                return Err(AdmissionRejection {
                    code: ERR_OPERATION_IN_PROGRESS,
                    message: format!(
                        "a stop/restart control operation is executing; retry after it \
                         reaches a terminal state (active: {})",
                        guard
                            .active_operation_id
                            .as_deref()
                            .or(guard.pending_stop.as_deref())
                            .unwrap_or("unknown")
                    ),
                    active_operation_id: guard
                        .active_operation_id
                        .clone()
                        .or_else(|| guard.pending_stop.clone()),
                });
            }
        }
        // 持久化受理（落盘失败不入执行队列——spec §3.3）。
        let stored = StoredOperation {
            view: RuntimeOperationView {
                operation_id: request.operation_id.clone(),
                kind: request.kind,
                state: RuntimeOperationState::Accepted,
                request_digest: digest,
                revision,
                runtime_instance_id: self.identity.runtime_instance_id.clone(),
                error_code: None,
                error_message: None,
                failure_detail: None,
            },
            request,
        };
        if let Err(error) = self.store.store_operation(&stored) {
            return Err(AdmissionRejection {
                code: "ERR_BACKEND_ERROR",
                message: format!("persist accepted operation: {error:#}"),
                active_operation_id: None,
            });
        }
        // stop 受理即持久化意图并推进 revision：旧构建稍后提交被拒（spec §3.3）。
        // R04：Accepted 已落盘后 desired 写失败 = 部分提交——该操作转为
        // RecoveryRequired 并挂恢复保护（终态可查询、执行不派发；其他 ID
        // 同样被拒，直至重启恢复裁决）。绝不能只返回错误留下永远 Accepted
        // 的幽灵记录。
        if is_stop {
            if let Err(error) = self
                .store
                .store_desired(DesiredState::Stopped, next_revision)
            {
                guard.recovery_protection = true;
                return Err(
                    self.hold_partial_admission(&stored, format!("persist stop intent: {error:#}"))
                );
            }
        } else if let Err(error) = self.store.store_desired(DesiredState::Running, revision) {
            guard.recovery_protection = true;
            return Err(
                self.hold_partial_admission(&stored, format!("persist running intent: {error:#}"))
            );
        }
        let mut queued = false;
        if is_stop {
            // R02：Stop 受理时清空排队启动（停止意图胜过排队——不得复活）
            if let Some(superseded) = guard.pending_restart.take()
                && let Err(error) = self.write_terminal(
                    &superseded,
                    RuntimeOperationState::Cancelled,
                    Some((
                        "ERR_SUPERSEDED".to_string(),
                        "superseded by a newer stop intent before execution".to_string(),
                    )),
                    None,
                )
            {
                // 终态落盘失败不得只打日志继续成功：恢复槽位并拒绝本次受理
                guard.pending_restart = Some(superseded);
                guard.recovery_protection = true;
                return Err(self.hold_partial_admission(
                    &stored,
                    format!("persist superseded terminal state: {error:#}"),
                ));
            }
            guard.queued_input = None;
            guard.pending_stop = Some(stored.view.operation_id.clone());
        } else if guard.active_operation_id.is_some() || guard.pending_stop.is_some() {
            // R02"最后受理生效"：排队槽单值——旧排队者收束 Superseded 语义
            // （Cancelled + ERR_SUPERSEDED），新受理者占槽
            if let Some(superseded) = guard
                .pending_restart
                .replace(stored.view.operation_id.clone())
                && let Err(error) = self.write_terminal(
                    &superseded,
                    RuntimeOperationState::Cancelled,
                    Some((
                        "ERR_SUPERSEDED".to_string(),
                        "superseded by a newer start/restart request".to_string(),
                    )),
                    None,
                )
            {
                // 终态落盘失败不得只打日志继续成功：恢复槽位并拒绝本次受理
                guard.pending_restart = Some(superseded);
                guard.recovery_protection = true;
                return Err(self.hold_partial_admission(
                    &stored,
                    format!("persist superseded terminal state: {error:#}"),
                ));
            }
            guard.queued_input = Some(stored.clone());
            queued = true;
        } else {
            // R02 补漏：无 active 时受理——先沉降滞留的旧排队者（失败/崩溃
            // 路径可能未清槽），绝不让更旧请求在本请求成功后被派发
            if let Err(error) =
                self.settle_queued_restart_on_terminal_failure_for_admission(&mut guard)
            {
                guard.recovery_protection = true;
                return Err(self.hold_partial_admission(&stored, error.message));
            }
            guard.active_operation_id = Some(stored.view.operation_id.clone());
        }
        let action = self.dispatch_action_for(&stored);
        guard.latest_admitted_operation_id = Some(stored.view.operation_id.clone());

        let operation_id = stored.view.operation_id.clone();
        self.emit(
            &operation_id,
            1,
            "accepted",
            None,
            Some(stored.view.kind.as_str()),
        );
        if queued {
            // 排队受理：不立即派发（active 收束时派发）。返回 Accepted——
            // 调用方知道操作已被受理排队（事件流可观察）。
            tracing::info!(
                operation_id,
                "operation admitted; queued behind active execution (last accepted wins)"
            );
            return Ok(AdmissionOutcome::Accepted(stored.view));
        }
        // 执行派发（server 主循环持有唯一执行权；内核不并发起第二个 worker）。
        (self.dispatch)(action);
        Ok(AdmissionOutcome::Accepted(stored.view))
    }

    pub(crate) async fn get(&self, operation_id: &str) -> Result<Option<RuntimeOperationView>> {
        Ok(self
            .store
            .load_operation(operation_id)?
            .map(|stored| stored.view))
    }

    /// Decide a foreground failure and cancellation under the same lock as
    /// admission. A successor that wins first retires the old observer; when
    /// exit wins first, later business requests are rejected before persistence.
    pub(crate) async fn begin_foreground_failure_exit(
        &self,
        operation_id: &str,
        cancel: impl FnOnce(),
    ) -> Result<Option<String>> {
        let mut guard = self.admission.lock().await;
        let stored = self
            .store
            .load_operation(operation_id)?
            .context("foreground operation record missing")?;
        anyhow::ensure!(
            stored.view.operation_id == operation_id
                && stored.request.operation_id == operation_id
                && stored.view.runtime_instance_id == self.identity.runtime_instance_id
                && stored.request.expected_runtime_instance_id == self.identity.runtime_instance_id
                && stored.request.workspace_id == self.identity.workspace_id
                && stored.view.request_digest
                    == runtime_request_digest(&stored.request).map_err(anyhow::Error::msg)?,
            "foreground operation identity changed"
        );
        if !matches!(
            stored.view.state,
            RuntimeOperationState::Failed | RuntimeOperationState::RecoveryRequired
        ) {
            return Ok(None);
        }
        let (_, revision) = self.store.load_desired()?;
        let other_request = guard
            .active_operation_id
            .iter()
            .chain(guard.pending_restart.iter())
            .chain(guard.pending_stop.iter())
            .any(|id| id != operation_id)
            || guard
                .queued_input
                .as_ref()
                .is_some_and(|queued| queued.view.operation_id != operation_id);
        if stored.request.expected_revision != revision
            || stored.view.revision != revision
            || guard.latest_admitted_operation_id.as_deref() != Some(operation_id)
            || other_request
        {
            return Ok(None);
        }
        let message = format!(
            "foreground operation {operation_id} failed: {}",
            stored
                .view
                .error_message
                .as_deref()
                .unwrap_or("runtime operation did not complete")
        );
        guard.foreground_exiting_operation = Some(operation_id.to_owned());
        // This synchronous cancellation must remain inside the admission
        // boundary. No newer operation can enter between the check and cancel.
        cancel();
        Ok(Some(message))
    }

    /// 终态发布（server 主循环在执行完成/失败后调用；先持久化再清 active 槽）。
    ///
    /// R04 终态单调：已有终态（Succeeded/Failed/Cancelled）或 RecoveryRequired
    /// 的操作不可被再次 finish 覆盖——迟到的 finish（同态幂等返回 Ok；异态
    /// 拒绝并保留原终态）只做身份/取消簿记清理。取消收束后的 Cancelled 记录
    /// 不会被迟到的成功/失败提交改写。
    pub(crate) async fn finish(
        &self,
        operation_id: &str,
        state: RuntimeOperationState,
        error: Option<(String, String)>,
        failure_detail: Option<RuntimeFailureDetail>,
        _next_sequence: u64,
    ) -> Result<()> {
        let mut guard = self.admission.lock().await;
        let stored = self.write_terminal(operation_id, state, error, failure_detail)?;
        if stored.view.state != state {
            // 已有终态未被覆盖（R04）：仅做簿记清理，不重复发终态事件
            tracing::warn!(
                operation_id,
                existing = ?stored.view.state,
                refused = ?state,
                "runtime operation already terminal; refusing overwrite (R04)"
            );
        }
        if let Ok(mut set) = self.cancelled.lock() {
            set.remove(operation_id);
        }
        // V03：恢复保护由**结果未知**决定——任何操作以 RecoveryRequired 收束
        // 都挂起保护，不依赖 active 恰好仍是该操作（Stop 待执行期间，旧执行者
        // A 的未知结果同样必须锁住写入口）。
        if stored.view.state == RuntimeOperationState::RecoveryRequired {
            guard.recovery_protection = true;
        }
        if guard.active_operation_id.as_deref() == Some(operation_id) {
            guard.active_operation_id = None;
            // A completed (including failed/cancelled) execution must not discard
            // a newer accepted start. Stop and uncertain cleanup remain barriers.
            if stored.view.state.is_terminal() && guard.pending_stop.is_none() {
                self.promote_queued(&mut guard)?;
            } else if stored.view.state == RuntimeOperationState::RecoveryRequired {
                self.settle_queued_restart_on_terminal_failure(
                    &mut guard,
                    "active failed, cancelled or unknown outcome",
                )?;
            }
        }
        if guard.pending_stop.as_deref() == Some(operation_id) {
            guard.pending_stop = None;
            if stored.view.state.is_terminal() && guard.active_operation_id.is_none() {
                self.promote_queued(&mut guard)?;
            } else if stored.view.state == RuntimeOperationState::RecoveryRequired {
                self.settle_queued_restart_on_terminal_failure(&mut guard, "stop cleanup unknown")?;
            }
        }
        Ok(())
    }

    /// 终态写入（无锁内聚版本；调用方自持 admission 锁时用
    /// [`Self::write_terminal_locked`]）。终态单调：已有终态/恢复保护不覆盖。
    pub(super) fn write_terminal(
        &self,
        operation_id: &str,
        state: RuntimeOperationState,
        error: Option<(String, String)>,
        failure_detail: Option<RuntimeFailureDetail>,
    ) -> Result<StoredOperation> {
        let mut stored = self
            .store
            .load_operation(operation_id)?
            .context("finish unknown operation")?;
        let terminal = state.is_terminal() || state == RuntimeOperationState::RecoveryRequired;
        anyhow::ensure!(terminal, "finish requires a terminal or recovery state");
        if stored.view.state.is_terminal()
            || stored.view.state == RuntimeOperationState::RecoveryRequired
        {
            self.emit_terminal_record(&stored);
            return Ok(stored);
        }
        stored.view.state = state;
        if let Some((code, message)) = error {
            stored.view.error_code = Some(code);
            stored.view.error_message = Some(message);
        }
        stored.view.failure_detail = failure_detail;
        self.store.store_operation(&stored)?;
        self.emit_terminal_record(&stored);
        Ok(stored)
    }

    pub(super) fn emit_terminal_record(&self, stored: &StoredOperation) {
        let event_name = if stored.view.state == RuntimeOperationState::Succeeded {
            "Completed"
        } else {
            "Failed"
        };
        let payload = stored.view.error_code.as_ref().map(|code| {
            serde_json::json!({
                "code": code,
                "error": stored.view.error_message.clone().unwrap_or_default(),
            })
        });
        self.emit_with_payload(
            &stored.view.operation_id,
            0,
            "terminal",
            None,
            Some(event_name),
            payload,
        );
    }

    pub(super) fn promote_queued(&self, guard: &mut AdmissionState) -> Result<()> {
        let Some(id) = guard.pending_restart.clone() else {
            return Ok(());
        };
        if guard.recovery_protection {
            return Ok(());
        }
        let loaded = self.store.load_operation(&id);
        let valid = matches!(&loaded, Ok(Some(op)) if
            op.view.state == RuntimeOperationState::Accepted && op.view.kind != RuntimeOperationKind::Stop);
        let input = guard
            .queued_input
            .as_ref()
            .filter(|op| op.view.operation_id == id);
        if !valid || input.is_none() {
            guard.recovery_protection = true;
            anyhow::bail!(
                "queued operation {id} lacks confirmed execution input; recovery required"
            );
        }
        let input = guard
            .queued_input
            .take()
            .context("queued execution input disappeared")?;
        let action = self.dispatch_action_for(&input);
        guard.pending_restart = None;
        guard.active_operation_id = Some(id.clone());
        self.emit(&id, 0, "dispatched", None, Some("QueuedDispatch"));
        (self.dispatch)(action);
        Ok(())
    }

    /// Cleanup remains unknown: settle the queued request explicitly rather than
    /// leave it permanently Accepted. Confirmed failure/cancellation promotes it.
    ///
    /// 落盘失败必须传播（不能只打日志留下永远 Accepted 的排队者）；
    /// 失败时把排队者放回槽位，下一次受理/收束路径重试沉降。
    pub(super) fn settle_queued_restart_on_terminal_failure(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, AdmissionState>,
        reason: &'static str,
    ) -> Result<()> {
        let Some(queued_id) = guard.pending_restart.take() else {
            return Ok(());
        };
        if let Err(error) = self.write_terminal(
            &queued_id,
            RuntimeOperationState::Cancelled,
            Some((
                "ERR_SUPERSEDED".to_string(),
                format!("superseded: active operation finished without success ({reason}); re-admit to retry"),
            )),
            None,
        ) {
            guard.pending_restart = Some(queued_id.clone());
            return Err(anyhow::anyhow!(
                "settle superseded queued operation: {error:#}"
            ));
        }
        guard.queued_input = None;
        Ok(())
    }

    /// admission 路径的滞留排队者沉降：落盘失败以 ERR_BACKEND_ERROR
    /// 拒绝新受理（槽位保持，下一次重试），不静默继续。
    pub(super) fn settle_queued_restart_on_terminal_failure_for_admission(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, AdmissionState>,
    ) -> Result<(), AdmissionRejection> {
        let Some(queued_id) = guard.pending_restart.clone() else {
            return Ok(());
        };
        if let Err(error) = self.write_terminal(
            &queued_id,
            RuntimeOperationState::Cancelled,
            Some((
                "ERR_SUPERSEDED".to_string(),
                "superseded by a newer request admitted while idle".to_string(),
            )),
            None,
        ) {
            return Err(AdmissionRejection {
                code: "ERR_BACKEND_ERROR",
                message: format!("settle superseded queued operation: {error:#}"),
                active_operation_id: None,
            });
        }
        guard.pending_restart = None;
        guard.queued_input = None;
        Ok(())
    }

    /// 持久化终态并在**已持有的 admission 锁内**完成身份/取消簿记——
    /// 提交线性化点唯一（R03：检查与写入之间不再有取消/受理窗口）。
    pub(super) fn write_terminal_locked(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, AdmissionState>,
        operation_id: &str,
        state: RuntimeOperationState,
    ) -> Result<()> {
        let stored = self.write_terminal(operation_id, state, None, None)?;
        if let Ok(mut set) = self.cancelled.lock() {
            set.remove(operation_id);
        }
        // V03：同 finish——保护由结果未知决定；两槽按身份清理
        if stored.view.state == RuntimeOperationState::RecoveryRequired {
            guard.recovery_protection = true;
        }
        if guard.active_operation_id.as_deref() == Some(operation_id) {
            guard.active_operation_id = None;
            // A completed (including failed/cancelled) execution must not discard
            // a newer accepted start. Stop and uncertain cleanup remain barriers.
            if stored.view.state.is_terminal() && guard.pending_stop.is_none() {
                self.promote_queued(guard)?;
            } else if stored.view.state == RuntimeOperationState::RecoveryRequired {
                self.settle_queued_restart_on_terminal_failure(
                    guard,
                    "active failed, cancelled or unknown outcome",
                )?;
            }
        }
        if guard.pending_stop.as_deref() == Some(operation_id) {
            guard.pending_stop = None;
            if stored.view.state.is_terminal() && guard.active_operation_id.is_none() {
                self.promote_queued(guard)?;
            } else if stored.view.state == RuntimeOperationState::RecoveryRequired {
                self.settle_queued_restart_on_terminal_failure(guard, "stop cleanup unknown")?;
            }
        }
        Ok(())
    }

    /// 执行提交屏障（B03）：在**同一 admission 锁**下原子裁决启动完成的提交。
    ///
    /// 线性化检查 + 终态写入（R03：全部在同一锁内完成，取消与成功提交不再
    /// 存在检查后写入的竞态窗口）：
    /// 1. 本操作仍是 active 执行者（未被并发收束/替换）；
    /// 2. 受理后无取消墓碑（有 → Cancelled，不报成功）；
    /// 3. 受理 revision 仍是当前 revision（Stop 受理会推进 revision——
    ///    推进过 → Superseded：调用方必须停服并让 Stop 执行，A 不报 Succeeded）。
    ///
    /// 通过 → 锁内原子收束 Succeeded。清理未知保持 RecoveryRequired 由调用方
    /// 经 [`Self::finish`] 表达。
    pub(crate) async fn commit_execution(
        &self,
        operation_id: &str,
    ) -> Result<CommitBarrierOutcome, anyhow::Error> {
        // 持久化用的终态在锁内决定并写入——提交线性化点
        let mut guard = self.admission.lock().await;
        if guard.active_operation_id.as_deref() != Some(operation_id) {
            return Ok(CommitBarrierOutcome::NotActive);
        }
        if self.is_cancelled(operation_id) {
            // V02：只观察取消意图，**不预写 Cancelled 终态**——调用方必须先
            // 确认业务/静态服务停止，再经 finish(Cancelled) 收束；清理未知时
            // 改走 RecoveryRequired（终态单调下 Cancelled 不可升级，预写会
            // 把未知结果锁死成已取消）。active/取消墓碑保留至收束。
            drop(guard);
            return Ok(CommitBarrierOutcome::Cancelled);
        }
        let (_, current_revision) = self.store.load_desired()?;
        let stored = self
            .store
            .load_operation(operation_id)?
            .context("commit unknown operation")?;
        if current_revision != stored.view.revision {
            // Stop 已受理（revision 推进）——本启动不得提交成功；
            // active 保留（Stop 信号在 server 循环执行并按自身 ID 收束后清除）。
            return Ok(CommitBarrierOutcome::Superseded);
        }
        let outcome =
            self.write_terminal_locked(&mut guard, operation_id, RuntimeOperationState::Succeeded);
        match outcome {
            Ok(()) => {
                drop(guard);
                Ok(CommitBarrierOutcome::Committed)
            }
            Err(error) => {
                // 终态持久化失败：保护保留（active 不清），调用方走恢复路径
                Err(error)
            }
        }
    }

    /// 取消请求（受理态返回；不伪称立即完成——spec §3.2 cancel 语义）。
    pub(crate) async fn request_cancel(&self, operation_id: &str) -> Result<bool> {
        let mut guard = self.admission.lock().await;
        if guard.active_operation_id.as_deref() != Some(operation_id) {
            return Ok(false);
        }
        // R03：真实取消——记录墓碑，执行侧在副作用边界
        // （[`Self::is_cancelled`]）检查并在终态提交前收束为 Cancelled。
        // 不在此改写状态：终态仍由执行边界持久化（含取消场景的清理证据）。
        let operation = self
            .store
            .load_operation(operation_id)?
            .context("active operation missing")?;
        let mut cancelled = self
            .cancelled
            .lock()
            .map_err(|_| anyhow::anyhow!("cancel registry lock poisoned"))?;
        // A successful cancel response means the intent survives owner restart.
        // A failed durable write is uncertain: still prevent success in this
        // process, and retain the operation's recovery fence.
        if let Err(error) = self.store.store_cancellation(&operation) {
            cancelled.insert(operation_id.to_string());
            guard.recovery_protection = true;
            return Err(error).context("persist cancellation intent");
        }
        cancelled.insert(operation_id.to_string());
        Ok(true)
    }

    /// 部分提交围栏（R04）：Accepted 落盘后意图写失败时调用——操作转
    /// RecoveryRequired（尽量落盘；落盘也失败时仅日志，保护已在内存），
    /// 并置恢复保护阻断后续写受理。
    pub(super) fn hold_partial_admission(
        &self,
        stored: &StoredOperation,
        reason: String,
    ) -> AdmissionRejection {
        let mut held = stored.clone();
        held.view.state = RuntimeOperationState::RecoveryRequired;
        held.view.error_code = Some(ERR_RECOVERY_REQUIRED.into());
        held.view.error_message = Some(reason.clone());
        if let Err(persist_error) = self.store.store_operation(&held) {
            tracing::error!(
                "partial admission hold persist failed (op {}): {persist_error:#}",
                stored.view.operation_id
            );
        } else {
            self.emit_terminal_record(&held);
        }
        AdmissionRejection {
            code: ERR_RECOVERY_REQUIRED,
            message: format!(
                "admission persisted but intent write failed; operation {} held for recovery: {reason}",
                stored.view.operation_id
            ),
            active_operation_id: Some(stored.view.operation_id.clone()),
        }
    }

    /// 恢复保护是否生效（B05：所有写入口共享——旧部署链/自动启动同样查询）。
    pub(crate) fn recovery_protection_active(&self) -> bool {
        self.admission
            .try_lock()
            .map(|guard| guard.recovery_protection)
            .unwrap_or(true) // 锁竞争期间保守视为保护生效（fail-closed）
    }

    /// 操作是否已被请求取消（执行侧副作用边界检查点）。
    pub(crate) fn is_cancelled(&self, operation_id: &str) -> bool {
        self.cancelled
            .lock()
            .map(|set| set.contains(operation_id))
            .unwrap_or(false)
    }

    /// 受理请求 → 执行派发动作映射（admit 与排队派发共用；组合已由
    /// 受理前置校验收窄，防御臂 fail-fast）。
    pub(super) fn dispatch_action_for(&self, stored: &StoredOperation) -> DispatchAction {
        match (&stored.request.kind, &stored.request.profile) {
            (
                RuntimeOperationKind::Deploy,
                shared_types::RunProfileInput::Artifact {
                    artifact: shared_types::ArtifactInput::Url { url, sha256 },
                },
            ) => DispatchAction::DeployArtifact {
                operation_id: stored.view.operation_id.clone(),
                url: url.clone(),
                sha256: sha256.clone(),
                pg: stored
                    .request
                    .run_config
                    .as_ref()
                    .and_then(|config| config.pg.clone()),
            },
            (
                RuntimeOperationKind::Deploy,
                shared_types::RunProfileInput::Artifact {
                    artifact: shared_types::ArtifactInput::ArtifactId { artifact_id },
                },
            ) => DispatchAction::DeployLocalArtifact {
                operation_id: stored.view.operation_id.clone(),
                artifact_id: artifact_id.clone(),
                sha256: None,
                pg: stored
                    .request
                    .run_config
                    .as_ref()
                    .and_then(|config| config.pg.clone()),
            },
            (RuntimeOperationKind::Stop, _) => DispatchAction::StopBusiness {
                operation_id: stored.view.operation_id.clone(),
            },
            (
                RuntimeOperationKind::Start | RuntimeOperationKind::Restart,
                shared_types::RunProfileInput::Source { .. },
            ) => DispatchAction::OrchestrateSource {
                operation_id: stored.view.operation_id.clone(),
                dev_profile: true,
                pg: stored
                    .request
                    .run_config
                    .as_ref()
                    .and_then(|config| config.pg.clone()),
            },
            (kind, profile) => {
                // 防御纵深：受理前置校验已拒绝未支持组合
                tracing::error!(
                    "internal: dispatch mapping reached for unvalidated combination                      {kind}/{profile:?}"
                );
                DispatchAction::StopBusiness {
                    operation_id: stored.view.operation_id.clone(),
                }
            }
        }
    }

    pub(super) fn emit(
        &self,
        operation_id: &str,
        sequence: u64,
        stage: &str,
        service: Option<String>,
        event_name: Option<&str>,
    ) {
        self.emit_with_payload(operation_id, sequence, stage, service, event_name, None);
    }

    /// 带 payload 的事件落盘（R06：Failed 终态事件携带错误详情，SSE 消费方
    /// 无需再查操作视图即可还原失败原因）。
    pub(super) fn emit_with_payload(
        &self,
        operation_id: &str,
        _sequence: u64,
        stage: &str,
        service: Option<String>,
        event_name: Option<&str>,
        payload: Option<serde_json::Value>,
    ) -> bool {
        let _writer = self
            .event_write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let history = match self.store.replay_events(operation_id, 0) {
            Ok(history) => history,
            Err(error) => {
                tracing::error!(operation_id, "read runtime event cursor: {error:#}");
                return false;
            }
        };
        // Terminal publication is idempotent and closes the operation stream.
        if history.iter().any(|event| event.stage == "terminal") {
            return false;
        }
        let Some(sequence) = history
            .last()
            .map_or(Some(1), |event| event.sequence.checked_add(1))
        else {
            tracing::error!(operation_id, "runtime event sequence exhausted");
            return false;
        };
        let record = RuntimeEventRecord {
            operation_id: operation_id.to_string(),
            sequence,
            runtime_instance_id: self.identity.runtime_instance_id.clone(),
            stage: stage.to_string(),
            service,
            event_name: event_name.map(str::to_string),
            payload,
        };
        if let Err(error) = self.store.append_event(&record) {
            // 事件落盘失败不阻断受理/执行主链（操作记录是权威），
            // 但必须可见——静默丢事件违反 spec §3.6。
            tracing::error!("runtime event persist failed (op {operation_id}): {error:#}");
            return false;
        }
        true
    }

    /// 追加服务级编排事件到**调用方捕获的执行操作**的 journal（R06 事件桥：owner 形态
    /// 下平台经运行 API 读事件流，stdout EVT 只被本地 spawn 路径消费）。
    /// 不读取异步准入锁：并发 stdout/stderr 与管理查询不能丢失日志事件。
    /// 返回是否已落盘（供桥接方观测丢弃）。
    pub fn append_orchestration_event(
        &self,
        operation_id: &str,
        stage: &str,
        service: Option<String>,
        event_name: &str,
        payload: Option<serde_json::Value>,
    ) -> bool {
        match self.store.load_operation(operation_id) {
            Ok(Some(_)) => {}
            Ok(None) => return false,
            Err(error) => {
                tracing::error!(
                    operation_id,
                    "read orchestration event operation: {error:#}"
                );
                return false;
            }
        }
        self.emit_with_payload(operation_id, 0, stage, service, Some(event_name), payload)
    }
}
