use super::*;

/// 等待结果三态（两引擎共用）。
pub(super) enum Next {
    /// 回外层等待（Failed/服务退出保持等待，可再部署）。
    Wait,
    Redeploy(Box<InitialAction>),
    Exit,
}

pub(super) struct PreparedActivation {
    prepared: crate::deploy::PreparedDeploy,
    workspace: std::path::PathBuf,
    target: Option<ExecutionTarget>,
    pg: Option<shared_types::StartPgCredential>,
    proxy: Option<crate::supervisor::PreparedProxy>,
}

/// 状态机主循环：初始动作（env 部署 / 卷上既有版本直接编排 / 空容器挂 Idle）→
/// 编排 supervise；期间可被新部署请求打断（停旧服务 → 换 code → 重新编排）。
pub(super) enum InitialAction {
    /// env/热部署触发：下载制品后编排。
    Deploy(DeployRequest),
    Prepared(PreparedActivation),
    /// Explicit Source switches back from .run to the canonical source root.
    Source,
    SourcePrepared(crate::supervisor::PreparedProxy),
    ExistingPrepared(crate::supervisor::PreparedProxy),
    /// 卷上既有 release.lock（Pod 重建恢复/激活目录复用）：跳过下载直接
    /// 编排。携带确认的执行目录——artifact 目标可能与当前执行目录不同
    ///（源码态 owner 切回 `.run`），编排必须落在该目录上。
    Existing {
        workspace: std::path::PathBuf,
    },
    /// 运行控制 stop：停止业务服务（保持管理面）。携带受理操作 ID——
    /// 从受理、排队、执行到终态完整传递（B01：Stop 不设 current，按
    /// 自身 ID 收束，绝不依赖"最近一次"全局值）。
    StopBusiness {
        operation_id: String,
    },
    /// 排队期已取消的操作：已按自身 ID 收束 Cancelled，零副作用——
    /// 不停止无关运行实例、不派发 Stop（R04：取消收束不得变成 Stop 执行）。
    Settled,
}

/// RV01：消费前核对原操作仍是有效执行者。信号在通道排队期间，其操作
/// 可能已被会话交接收束（prepare_relaunch / 无驱动看门狗 / 迟到 finish /
/// 被更新受理取代）——终态或恢复保护的操作不得再执行；记录缺失或不可读
/// 同样不执行（无法核验的请求 fail-closed 跳过，不伪造其结果）。
pub(super) async fn signal_operation_already_settled(
    state: &ServerState,
    operation_id: &str,
) -> bool {
    let Some(kernel) = state.runtime_kernel() else {
        return false;
    };
    match kernel.get(operation_id).await {
        Ok(Some(view)) => {
            if view.state.is_terminal()
                || view.state == shared_types::RuntimeOperationState::RecoveryRequired
            {
                tracing::warn!(
                    operation_id,
                    state = ?view.state,
                    "runtime control signal skipped: operation no longer executable"
                );
                true
            } else {
                match kernel.mark_execution_consumed(operation_id).await {
                    Ok(consumed) => !consumed,
                    Err(error) => {
                        tracing::error!(%error, operation_id,
                            "runtime control signal could not claim its original execution");
                        true
                    }
                }
            }
        }
        Ok(None) => {
            tracing::warn!(
                operation_id,
                "runtime control signal skipped: operation record missing"
            );
            true
        }
        Err(error) => {
            tracing::error!(
                %error,
                operation_id,
                "runtime control signal skipped: operation record unreadable"
            );
            true
        }
    }
}

/// 控制信号在服务已停止后的收束（R01）：Reorchestrate 占据执行身份由外层
/// 重编排；Stopped 按信号自身 ID 收束（不触碰在执行的其他操作）。
/// C4：热载兼容**预检**——新候选对 active 的 server 拓扑兼容性在任何摘流/
/// 停旧之前判定（orchestrate 内部的同名校验仍保留为纵深防御）。不兼容 =
/// 操作受理后即拒：旧业务保持运行（standby 都不发——那是拆流，会中断
/// 服务），操作按 Failed 收束，走完整重配置流程。
pub(super) async fn preflight_hot_reload_compat(
    args: &crate::RuntimeArgs,
    dev_profile: bool,
) -> anyhow::Result<crate::supervisor::PreparedProxy> {
    let release = crate::manifest::read_release_lock(&args.workspace)
        .context("read release lock for hot-reload preflight")?;
    let runtime_root = crate::proxy::compiler::runtime_root(&args.log_dir);
    let outcome = crate::proxy::compiler::compile_and_validate(
        &args.workspace,
        &runtime_root,
        &args.pingap_bin,
        &release,
        dev_profile,
    )
    .await
    .context("compile and validate candidate before stopping predecessor")?;
    let active = crate::proxy::compiler::active_config_path(&runtime_root);
    let candidate = tokio::fs::read_to_string(&outcome.config_path)
        .await
        .context("read validated proxy candidate")?;
    crate::proxy::compiler::validate_hot_reload_compatible(&active, &candidate)?;
    Ok(crate::supervisor::PreparedProxy {
        workspace: args.workspace.clone(),
        release,
        dev_profile,
        outcome,
    })
}

async fn prepare_source_control(
    owner_args: &RuntimeArgs,
    state: &ServerState,
    signal: &ControlSignal,
) -> anyhow::Result<Option<crate::supervisor::PreparedProxy>> {
    let ControlSignal::OrchestrateSource { dev_profile, .. } = signal else {
        return Ok(None);
    };
    let mut source_args = owner_args.clone();
    source_args.workspace =
        resolved_execution_workspace(&owner_args.workspace, Some(ExecutionTarget::Source), state)?;
    preflight_hot_reload_compat(&source_args, *dev_profile)
        .await
        .map(Some)
}

async fn reject_live_control(state: &ServerState, signal: &ControlSignal, error: &anyhow::Error) {
    let operation_id = match signal {
        ControlSignal::OrchestrateSource { operation_id, .. }
        | ControlSignal::StopBusiness { operation_id } => operation_id,
    };
    let unknown = error
        .downcast_ref::<supervisor::ShutdownUnconfirmed>()
        .is_some();
    if let Err(persist_error) = state
        .finish_runtime_operation_by_id(
            operation_id,
            if unknown {
                shared_types::RuntimeOperationState::RecoveryRequired
            } else {
                shared_types::RuntimeOperationState::Failed
            },
            Some(("entry_preflight_failed".into(), format!("{error:#}"))),
        )
        .await
    {
        state.begin_failure(
            format!("entry preflight rejection could not be recorded: {persist_error}"),
            true,
        );
    }
    if unknown {
        state.ready.set_ready(false);
        state.begin_failure(
            format!("entry restoration unknown; business retained: {error:#}"),
            true,
        );
    }
    // Preserve the existing driver, execution directory and readiness. The
    // rejected operation must not route a second request through Idle.
}

async fn reject_live_deployment(
    state: &ServerState,
    error: anyhow::Error,
    predecessor_ready: bool,
) {
    if error
        .downcast_ref::<supervisor::ShutdownUnconfirmed>()
        .is_some()
    {
        if let Err(persist) = state
            .finish_current_runtime_operation(
                shared_types::RuntimeOperationState::RecoveryRequired,
                Some((
                    shared_types::ERR_RECOVERY_REQUIRED.into(),
                    format!("{error:#}"),
                )),
            )
            .await
        {
            tracing::error!(%persist, "unknown entry result could not be persisted");
        }
        state.ready.set_ready(false);
        state.begin_failure(
            format!("entry restoration unknown; business retained: {error:#}"),
            true,
        );
    } else {
        fail_preparation(state, format!("entry drain: {error:#}")).await;
        state.set_phase(ServerPhase::Running);
        state.ready.set_ready(predecessor_ready);
    }
}

async fn claim_live_source_control(state: &ServerState, signal: &ControlSignal) -> bool {
    match signal {
        ControlSignal::OrchestrateSource { operation_id, .. } => {
            !state.settle_cancelled_before_execution(operation_id).await
                && !signal_operation_already_settled(state, operation_id).await
        }
        ControlSignal::StopBusiness { operation_id } => match state.runtime_kernel() {
            None => true,
            Some(kernel) => match kernel.get(operation_id).await {
                Ok(Some(view)) => {
                    !view.state.is_terminal()
                        && view.state != shared_types::RuntimeOperationState::RecoveryRequired
                }
                _ => false,
            },
        },
    }
}

async fn source_cancelled_after_preflight(state: &ServerState, signal: &ControlSignal) -> bool {
    match signal {
        ControlSignal::OrchestrateSource { operation_id, .. } => {
            state.settle_cancelled_before_execution(operation_id).await
        }
        _ => false,
    }
}

fn restore_running_predecessor(state: &ServerState, ready: bool) {
    let protected = state.runtime_recovery_hold_active()
        || state
            .deploy_status()
            .operation
            .as_ref()
            .and_then(|operation| operation.recovery.as_ref())
            .is_some_and(|recovery| recovery.status == "pending");
    if !protected && !state.is_cancelled() {
        state.set_phase(ServerPhase::Running);
        state.ready.set_ready(ready);
    }
}

async fn settled_prepared_control(
    state: &ServerState,
    signal: ControlSignal,
    prepared: Option<crate::supervisor::PreparedProxy>,
) -> InitialAction {
    let action = settle_control_signal_inner(state, signal, true).await;
    match (action, prepared) {
        (InitialAction::Source, Some(proxy)) => InitialAction::SourcePrepared(proxy),
        (action, _) => action,
    }
}

pub(super) async fn settle_control_signal(
    state: &ServerState,
    signal: ControlSignal,
) -> InitialAction {
    settle_control_signal_inner(state, signal, false).await
}

async fn settle_control_signal_inner(
    state: &ServerState,
    signal: ControlSignal,
    source_consumed: bool,
) -> InitialAction {
    match signal {
        ControlSignal::OrchestrateSource {
            operation_id,
            dev_profile,
            pg,
        } => {
            // R03/R04 取消检查点：派发排队期间被取消 → 按自身 ID 收束
            // Cancelled 后零动作返回——不编排、不派发 Stop（取消收束变成
            // Stop 执行会用 Succeeded 覆盖 Cancelled 并停止无关运行实例）
            if !source_consumed && state.settle_cancelled_before_execution(&operation_id).await {
                return InitialAction::Settled;
            }
            // RV01：会话交接/取代路径已收束的操作不执行（旧信号不能执行
            // 已失败/已取消/已恢复保护的请求）。
            if !source_consumed && signal_operation_already_settled(state, &operation_id).await {
                return InitialAction::Settled;
            }
            state.set_current_runtime_operation(Some(operation_id.clone()));
            let request = DeployRequest {
                runtime_operation_id: Some(operation_id.clone()),
                url: "source://workspace".into(),
                release_id: format!("runtime-{operation_id}"),
                sha256: None,
                local_path: None,
                execution_target: Some(ExecutionTarget::Source),
                // Internal journal retains actual credentials for restart
                // recovery; APIs expose typed views.
                run_pg: pg.clone(),
            };
            if let Err(error) = state.record_runtime_deployment(&request) {
                hold_unconfirmed(state, format!("persist source execution: {error:#}")).await;
                return InitialAction::Settled;
            }
            state.set_pending_dev_profile(dev_profile);
            state.set_pending_run_config(pg);
            InitialAction::Source
        }
        ControlSignal::StopBusiness { operation_id } => {
            InitialAction::StopBusiness { operation_id }
        }
    }
}

pub(super) async fn settle_stopped_startup(
    args: &RuntimeArgs,
    state: &ServerState,
    signal: ControlSignal,
    reason: String,
) -> InitialAction {
    finish_interrupted_activation(
        args,
        state,
        reason,
        shared_types::RuntimeOperationState::Cancelled,
    )
    .await;
    settle_control_signal(state, signal).await
}

pub(super) async fn record_uncertain_control(
    state: &ServerState,
    signal: &ControlSignal,
    error: &str,
) {
    let operation_id = match signal {
        ControlSignal::OrchestrateSource { operation_id, .. }
        | ControlSignal::StopBusiness { operation_id } => operation_id,
    };
    // A successfully running predecessor has already cleared current_runtime_operation.
    // Bind a stop failure to the consumed request explicitly, before the global hold.
    if let Err(persist_error) = state
        .finish_runtime_operation_by_id(
            operation_id,
            shared_types::RuntimeOperationState::RecoveryRequired,
            Some((shared_types::ERR_RECOVERY_REQUIRED.into(), error.into())),
        )
        .await
    {
        tracing::error!(%persist_error, "persist uncertain control failed; recovery hold retained");
    }
}

/// Prepare while the existing supervisor continues serving. Failed requests do
/// not leave this wait loop and never reach the stop/activate boundary.
/// `current_workspace` is the execution directory this loop is presently
/// orchestrating (may differ from the owner's serve invocation directory after
/// an artifact switch).
pub(super) async fn next_prepared(
    owner_args: &RuntimeArgs,
    current_workspace: &std::path::Path,
    state: &Arc<ServerState>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<DeployRequest>,
) -> Option<InitialAction> {
    loop {
        let predecessor_ready = state.ready.is_ready();
        let request = rx.recv().await?;
        if let Some(id) = &request.runtime_operation_id
            && signal_operation_already_settled(state, id).await
        {
            // RV01：会话交接后残留的旧部署信号——其操作已收束，不执行。
            continue;
        }
        if let Some(id) = &request.runtime_operation_id {
            state.set_current_runtime_operation(Some(id.clone()));
        }
        if let Err(error) = state.record_runtime_deployment(&request) {
            hold_unconfirmed(state, format!("persist deployment execution: {error:#}")).await;
            return None;
        }
        let workspace = match resolved_execution_workspace(
            &owner_args.workspace,
            request.execution_target,
            state,
        ) {
            Ok(workspace) => workspace,
            Err(error) => {
                fail_preparation(state, format!("resolve deployment workspace: {error:#}")).await;
                restore_running_predecessor(state, predecessor_ready);
                continue;
            }
        };
        let target = request.execution_target;
        let pg = request.run_pg.clone();
        let state_clone = state.clone();
        let progress_cb: crate::deploy::ProgressCallback =
            std::sync::Arc::new(move |p: shared_types::AppDeploymentProgress| {
                state_clone.set_deploy_progress(p);
            });
        match state
            .preparations
            .run(workspace.clone(), request, Some(progress_cb))
            .await
        {
            Ok(Some(prepared)) => {
                let proxy = match prepare_artifact_proxy(
                    owner_args,
                    prepared.validated_workspace(),
                    &workspace,
                    target == Some(ExecutionTarget::Source),
                )
                .await
                {
                    Ok(proxy) => proxy,
                    Err(error) => {
                        fail_preparation(state, format!("proxy preflight: {error:#}")).await;
                        restore_running_predecessor(state, predecessor_ready);
                        continue;
                    }
                };
                if state.current_operation_cancelled() {
                    drop(prepared);
                    if let Err(error) = state.fail_operation(
                        "deployment cancelled before activation".into(),
                        Boundary::Preparing,
                    ) {
                        hold_unconfirmed(state, format!("persist cancellation: {error:#}")).await;
                        return None;
                    }
                    if let Some(id) = state.current_runtime_operation() {
                        state.settle_cancelled_before_execution(&id).await;
                    }
                    restore_running_predecessor(state, predecessor_ready);
                    continue;
                }
                // Prepared content is not activated yet. The execution loop
                // persists Switching after shutdown, immediately before activate.
                return Some(InitialAction::Prepared(PreparedActivation {
                    prepared,
                    workspace,
                    target,
                    pg,
                    proxy: Some(proxy),
                }));
            }
            Ok(None) => match crate::manifest::read_release_lock(&workspace) {
                Ok(release) => {
                    state.set_release(release);
                    // The artifact cache only proves that files are unchanged.
                    // Explicit operation credentials still require a business
                    // restart; publishing Running here would acknowledge a
                    // configuration that the serving processes never received.
                    // A resolved target different from the current execution
                    // directory (source owner switching to a reused `.run`)
                    // must re-orchestrate on that directory, not record a
                    // no-op success while something else keeps serving.
                    if pg.is_some() || current_workspace != workspace {
                        let mut target_args = owner_args.clone();
                        target_args.workspace = workspace.clone();
                        let proxy = match preflight_hot_reload_compat(
                            &target_args,
                            target == Some(ExecutionTarget::Source),
                        )
                        .await
                        {
                            Ok(proxy) => proxy,
                            Err(error) => {
                                fail_preparation(state, format!("proxy preflight: {error:#}"))
                                    .await;
                                restore_running_predecessor(state, predecessor_ready);
                                continue;
                            }
                        };
                        if let Err(error) = state.complete_stage() {
                            fail_preparation(
                                state,
                                format!("persist unchanged artifact: {error:#}"),
                            )
                            .await;
                            continue;
                        }
                        state.set_pending_dev_profile(target == Some(ExecutionTarget::Source));
                        state.set_pending_run_config(pg);
                        return Some(InitialAction::ExistingPrepared(proxy));
                    }
                    if let Err(error) = state
                        .complete_stage()
                        .and_then(|()| state.complete_running())
                    {
                        fail_preparation(state, format!("persist unchanged deployment: {error:#}"))
                            .await;
                    }
                }
                Err(error) => {
                    fail_preparation(state, format!("read unchanged release: {error:#}")).await;
                    restore_running_predecessor(state, predecessor_ready);
                }
            },
            Err(error) => {
                fail_preparation(state, format!("prepare: {error:#}")).await;
                restore_running_predecessor(state, predecessor_ready);
            }
        }
    }
}

async fn prepare_artifact_proxy(
    owner_args: &RuntimeArgs,
    read_workspace: &std::path::Path,
    execution_workspace: &std::path::Path,
    dev_profile: bool,
) -> anyhow::Result<crate::supervisor::PreparedProxy> {
    let release = crate::manifest::read_release_lock(read_workspace)?;
    let root = crate::proxy::compiler::runtime_root(&owner_args.log_dir);
    let outcome = crate::proxy::compiler::compile_prepared(
        read_workspace,
        execution_workspace,
        &root,
        &owner_args.pingap_bin,
        &release,
        dev_profile,
    )
    .await?;
    let candidate = tokio::fs::read_to_string(&outcome.config_path).await?;
    crate::proxy::compiler::validate_hot_reload_compatible(
        &crate::proxy::compiler::active_config_path(&root),
        &candidate,
    )?;
    Ok(crate::supervisor::PreparedProxy {
        workspace: execution_workspace.into(),
        release,
        dev_profile,
        outcome,
    })
}

pub(super) async fn fail_preparation(state: &ServerState, error: String) {
    if let Err(settle_error) = state
        .finish_current_runtime_operation(
            shared_types::RuntimeOperationState::Failed,
            Some(("ERR_BACKEND_ERROR".to_string(), error.clone())),
        )
        .await
    {
        // V04：失败终态都写不进——结果未知，走恢复保护路径
        hold_unconfirmed(state, format!("{error}; {settle_error}")).await;
        return;
    }
    if state.preparations.is_poisoned() {
        hold_unconfirmed(state, error).await;
        return;
    }
    if let Err(persist_error) = state.fail_operation(error.clone(), Boundary::Preparing) {
        hold_unconfirmed(
            state,
            format!("{error}; persist preparation failure: {persist_error:#}"),
        )
        .await;
    }
}

/// Keep the API alive and admission closed when a writer may still be active.
/// No recovery directory writes or releasable terminal status follow this point.
pub(super) async fn hold_unconfirmed(state: &ServerState, error: String) {
    if let Err(settle_error) = state
        .finish_current_runtime_operation(
            shared_types::RuntimeOperationState::RecoveryRequired,
            Some((
                shared_types::ERR_RECOVERY_REQUIRED.to_string(),
                error.clone(),
            )),
        )
        .await
    {
        // V04：连 RecoveryRequired 都不可持久化——身份保留 + server 门禁
        // （finish 内已挂起）；此处如实记录后维持保护现场
        tracing::error!("{settle_error}; holding without durable terminal state");
    }
    state.ready.set_ready(false);
    state.begin_failure(error.clone(), true);
    tracing::error!(%error, "Deployment remains pending until process shutdown is confirmed; operator recovery required");
    state.cancel_token().cancelled().await;
}

pub(super) async fn fail_activation(
    args: &RuntimeArgs,
    state: &ServerState,
    error: String,
) -> Option<InitialAction> {
    finish_interrupted_activation(
        args,
        state,
        error,
        shared_types::RuntimeOperationState::Failed,
    )
    .await
}

/// The caller has joined the orchestration task and confirmed process cleanup.
/// Unknown physical cleanup or state persistence retains recovery protection.
pub(super) async fn finish_interrupted_activation(
    args: &RuntimeArgs,
    state: &ServerState,
    error: String,
    terminal: shared_types::RuntimeOperationState,
) -> Option<InitialAction> {
    // R06：先清理、确认后才发布终态——原顺序先记 Failed 再清理，清理未知
    // 时操作身份已被清除，恢复保护无从挂起。清理失败路径经 hold_unconfirmed
    // 以 RecoveryRequired 收束（身份保留至该点），成功路径最后记 Failed。
    state.ready.set_ready(false);
    if let Err(error) = state.preparations.drain().await {
        hold_unconfirmed(state, format!("preparation shutdown: {error:#}")).await;
        return None;
    }
    if let Err(stop_error) = crate::static_hosting::reconcile(&[], &args.workspace, false).await {
        hold_unconfirmed(state, format!("{error}; static shutdown: {stop_error:#}")).await;
        return None;
    }
    let artifact = crate::manifest::read_release_lock(&args.workspace)
        .ok()
        .map(|release| release.release_id);
    let boundary = state
        .journal
        .lock()
        .map(|journal| {
            let receipt = journal
                .as_ref()
                .and_then(|journal| journal.receipt.as_ref());
            if receipt.is_some_and(|receipt| {
                matches!(
                    receipt.boundary,
                    Boundary::Activated | Boundary::Active | Boundary::StartupFailed
                ) && artifact.is_some()
                    && receipt.operation.artifact_release_id == artifact
            }) {
                Boundary::StartupFailed
            } else {
                Boundary::Failed
            }
        })
        .map_err(|_| ());
    let boundary = match boundary {
        Ok(boundary) => boundary,
        Err(()) => {
            hold_unconfirmed(state, format!("{error}; deployment journal lock poisoned")).await;
            return None;
        }
    };
    if let Err(persist_error) = state.fail_operation(error.clone(), boundary) {
        hold_unconfirmed(
            state,
            format!("{error}; persist failure: {persist_error:#}"),
        )
        .await;
        return None;
    }
    if let Err(settle_error) = state
        .finish_current_runtime_operation(
            terminal,
            Some(("ERR_BACKEND_ERROR".to_string(), error.clone())),
        )
        .await
    {
        // V04：清理已确认但失败终态写不进——结果未知，保持恢复保护
        tracing::error!("{settle_error}; operation held for recovery");
    }
    None
}

/// 屏障裁决后的取消原因辅助（V02）：仅用于收束记录的 reason 文案；
/// 权威判定已在屏障内完成。
pub(super) fn commit_running_barrier_reason_cancelled(state: &ServerState) -> bool {
    state.current_operation_cancelled()
}

/// 内核提交屏障结果（B03 收敛形态）。
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub(super) enum BarrierOutcome {
    /// 屏障通过（已收束 Succeeded）/ 无在途操作——调用方继续正常流转。
    Passed,
    /// 观察到取消意图（V02：内核**未写终态**）——调用方必须先停服并确认
    /// 清理，再按 ID 收束 Cancelled；清理未知走 RecoveryRequired。
    CancelledByRequest,
    /// Stop 已受理（revision 推进）：调用方必须停服、按 ID 收束 Cancelled。
    Superseded,
}

/// Running 入口的统一提交屏障（B03）：无内核/无在途操作时直通。
/// 屏障失败（持久化错误）→ 按原终态语义收束 RecoveryRequired 并视为 Passed
/// （fail-closed：不报成功，保护保留在内核）。
///
/// R01：屏障通过即清理**server 侧执行身份**（内核 finish 只清内核 active 槽，
/// 残留身份会让后续 Restart B 的 set_current 被旧 A 拒绝、B 完成时误收束 A，
/// B 永久 Accepted）。
/// R02：NotActive 不等同提交成功——操作已终态（其他路径收束）时幂等放行；
/// 非终态则 fail-closed 收束 RecoveryRequired（执行身份丢失，不报成功）。
pub(super) async fn commit_running_barrier(state: &ServerState) -> BarrierOutcome {
    let Some(kernel) = state.runtime_kernel() else {
        return BarrierOutcome::Passed;
    };
    let Some(operation_id) = state.current_runtime_operation() else {
        return BarrierOutcome::Passed;
    };
    let settle_identity = || {
        if state.current_runtime_operation().as_deref() == Some(operation_id.as_str()) {
            state.set_current_runtime_operation(None);
        }
    };
    match kernel.commit_execution(&operation_id).await {
        Ok(crate::runtime_kernel::CommitBarrierOutcome::Committed) => {
            // R01：内核已收束 Succeeded——同步清理 server 执行身份
            settle_identity();
            BarrierOutcome::Passed
        }
        Ok(crate::runtime_kernel::CommitBarrierOutcome::Cancelled) => {
            // V02：取消意图已观察到但**终态未写**——不清身份；调用方先停服，
            // 确认后按 ID 收束（清理未知 → RecoveryRequired）
            BarrierOutcome::CancelledByRequest
        }
        Ok(crate::runtime_kernel::CommitBarrierOutcome::Superseded) => BarrierOutcome::Superseded,
        Ok(crate::runtime_kernel::CommitBarrierOutcome::NotActive) => {
            match kernel.get(&operation_id).await {
                Ok(Some(view)) if view.state.is_terminal() => {
                    // 幂等：操作已被其他路径收束——清理残留身份后放行
                    settle_identity();
                    BarrierOutcome::Passed
                }
                _ => {
                    // R02：active 不在本操作且无终态——执行身份丢失，fail-closed
                    tracing::error!(
                        "runtime commit barrier: operation {operation_id} lost execution \
                         identity before commit; settling RecoveryRequired"
                    );
                    if let Err(settle_error) = state
                        .finish_runtime_operation_by_id(
                            &operation_id,
                            shared_types::RuntimeOperationState::RecoveryRequired,
                            Some((
                                shared_types::ERR_RECOVERY_REQUIRED.into(),
                                "execution identity lost before commit barrier".into(),
                            )),
                        )
                        .await
                    {
                        tracing::error!("{settle_error}; recovery hold engaged");
                    }
                    BarrierOutcome::Passed
                }
            }
        }
        Err(error) => {
            tracing::error!("runtime commit barrier failed (op {operation_id}): {error:#}");
            if let Err(settle_error) = state
                .finish_runtime_operation_by_id(
                    &operation_id,
                    shared_types::RuntimeOperationState::RecoveryRequired,
                    Some((
                        shared_types::ERR_RECOVERY_REQUIRED.into(),
                        format!("commit barrier persistence failed: {error:#}"),
                    )),
                )
                .await
            {
                tracing::error!("{settle_error}; recovery hold engaged");
            }
            BarrierOutcome::Passed
        }
    }
}

async fn wait_supervisord_running(
    owner_args: &RuntimeArgs,
    args: &RuntimeArgs,
    state: &Arc<ServerState>,
    host: &SupervisordHost,
    hot_rx: &mut tokio::sync::mpsc::UnboundedReceiver<DeployRequest>,
    standby_root: &std::path::Path,
) -> Result<Option<InitialAction>> {
    let mut control_rx = state.control_rx.lock().await;
    loop {
        let predecessor_ready = state.ready.is_ready();
        let mut drained = false;
        let next = tokio::select! {
            maybe = next_prepared(owner_args, &args.workspace, state, hot_rx) => match maybe {
                Some(action) => Next::Redeploy(Box::new(action)),
                None => Next::Exit,
            },
            signal = control_rx.recv() => match signal {
                Some(signal) => {
                    if !claim_live_source_control(state, &signal).await { continue; }
                    let prepared = match prepare_source_control(owner_args, state, &signal).await {
                        Ok(prepared) => prepared,
                        Err(error) => { reject_live_control(state, &signal, &error).await; continue; }
                    };
                    if source_cancelled_after_preflight(state, &signal).await { continue; }
                    if let Err(error) = host.drain_entry(standby_root).await {
                        reject_live_control(state, &signal, &error).await;
                        continue;
                    }
                    drained = true;
                    let action = settled_prepared_control(state, signal, prepared).await;
                    if matches!(action, InitialAction::Settled) { continue; }
                    Next::Redeploy(Box::new(action))
                }
                None => Next::Wait,
            },
            () = async { state.cancel_token().cancelled().await } => Next::Exit,
        };
        match next {
            Next::Exit => {
                host.finish_business_session(Some(standby_root)).await?;
                return Ok(None);
            }
            Next::Wait => continue,
            Next::Redeploy(action) => {
                // Hot Deploy and control signals share the same barrier.
                // Failed standby preparation keeps this Running driver.
                if !drained && let Err(error) = host.drain_entry(standby_root).await {
                    reject_live_deployment(state, error, predecessor_ready).await;
                    continue;
                }
                state.ready.set_ready(false);
                if let Err(error) = host.stop_business().await {
                    hold_unconfirmed(state, format!("stop before activation failed: {error:#}"))
                        .await;
                    return Ok(None);
                }
                return Ok(Some(*action));
            }
        }
    }
}

pub(super) async fn server_loop(
    owner_args: &RuntimeArgs,
    state: &Arc<ServerState>,
    host: Option<SupervisordHost>,
    first: Option<InitialAction>,
    foreground: bool,
) -> Result<()> {
    // P1：所有业务停止路径统一先发布 standby 摘流（runtime_root 固定）。
    let standby_root = crate::proxy::compiler::runtime_root(&owner_args.log_dir);
    if let Some(host) = host.as_ref() {
        host.prepare_entry_baseline(&standby_root).await?;
    }

    let mut pending = first;
    let mut active_args = if pending.is_none() {
        // Idle management has no business runtime to resume. A fresh Source or
        // Deploy action selects its own directory; Stop needs no old manifest.
        owner_args.clone()
    } else {
        match restored_runtime_args_inner(owner_args, state, false) {
            Ok(args) => args,
            Err(error) => {
                if !state.source_replacement_hold_only() {
                    state.begin_runtime_recovery_hold();
                }
                state.begin_failure(
                    format!("runtime execution target recovery failed: {error:#}"),
                    !state.source_replacement_hold_only(),
                );
                // Keep consuming Stop and explicit replacement requests even when
                // automatic restoration cannot choose the old execution directory.
                pending = None;
                owner_args.clone()
            }
        }
    };
    let args = &mut active_args;
    loop {
        if state.is_cancelled() {
            return Ok(());
        }
        // 取下一个动作：有待处理的直接用，否则挂 Idle 等受理/信号
        let action = match pending.take() {
            Some(action) => action,
            None => {
                // Failed 不被 Idle 覆盖：保留失败痕迹（deploy_status.error）与
                // 摘流态（/ready 503），直到下一次部署请求进来
                if !matches!(state.phase(), ServerPhase::Failed(_)) {
                    state.set_phase(ServerPhase::Idle);
                }
                let mut rx = state.deploy_rx.lock().await;
                let mut control = state.control_rx.lock().await;
                let action = tokio::select! {
                    maybe = rx.recv() => match maybe {
                        Some(req) => Some(InitialAction::Deploy(req)),
                        None => return Ok(()), // api 层全退（不可能，防御）
                    },
                    signal = control.recv() => match signal {
                        Some(ControlSignal::OrchestrateSource { operation_id, dev_profile, pg }) => {
                            // B01：ID 必须完整传递——Idle 消费即占据执行身份
                            //（取消检查点在 settle_control_signal 内）
                            Some(settle_control_signal(
                                state,
                                ControlSignal::OrchestrateSource { operation_id, dev_profile, pg },
                            ).await)
                        }
                        Some(ControlSignal::StopBusiness { operation_id }) => {
                            Some(InitialAction::StopBusiness { operation_id })
                        }
                        None => {
                            // api 层全退（防御）；部署通道仍存活时继续等
                            tokio::select! {
                                maybe = rx.recv() => match maybe {
                                    Some(req) => Some(InitialAction::Deploy(req)),
                                    None => return Ok(()),
                                },
                                () = async { state.cancel_token().cancelled().await } => return Ok(()),
                            }
                        }
                    },
                    () = async { state.cancel_token().cancelled().await } => return Ok(()),
                };
                // control/防御分支产出 Option；展开为统一 InitialAction
                match action {
                    Some(action) => action,
                    None => continue,
                }
            }
        };

        let run_migrations = true;
        if matches!(action, InitialAction::Settled) {
            // R04：排队期取消已收束——零副作用回 Idle
            if !matches!(state.phase(), ServerPhase::Failed(_)) {
                state.set_phase(ServerPhase::Idle);
            }
            continue;
        }
        if let InitialAction::StopBusiness { operation_id } = action {
            // RV01：消费前核对——Stop 信号排队期间可能已被交接收束
            //（prepare_relaunch 幂等成功/看门狗按已确认无执行完成），
            // 重复执行停止无副作用但会以陈旧相位覆盖收束后的状态。
            if signal_operation_already_settled(state, &operation_id).await {
                if !matches!(state.phase(), ServerPhase::Failed(_)) {
                    state.set_phase(ServerPhase::Idle);
                }
                continue;
            }
            // stop：停止业务服务（保持管理面与常驻入口）。B01：按**自身受理
            // ID**收束——Stop 从不占据 current 执行槽，禁止 finish_current
            //（它会读 current=None 而静默丢终态，Stop 永远 Accepted）。
            // P1/V2-03：先发布 standby 并确认热载（摘流），再停业务；发布
            // 失败 = 停机失败（Err 保持旧服务可证实地运行）。
            let stopped = async {
                if let Some(host) = host.as_ref() {
                    host.stop_all(Some(&standby_root)).await?;
                }
                crate::static_hosting::reconcile(&[], &args.workspace, false).await
            }
            .await;
            match stopped {
                Ok(()) => {
                    state.restore_credentials_after_stop();
                    // V04：先持久化终态再切相位——收束失败（结果未知）不得
                    // 以 Idle 成功面貌继续
                    match state
                        .finish_runtime_operation_by_id(
                            &operation_id,
                            shared_types::RuntimeOperationState::Succeeded,
                            None,
                        )
                        .await
                    {
                        Ok(()) => state.set_phase(ServerPhase::Idle),
                        Err(settle_error) => {
                            tracing::error!("{settle_error}");
                            state.set_phase(ServerPhase::Failed(settle_error.clone()));
                            if foreground {
                                return Err(anyhow::anyhow!(settle_error))
                                    .context("foreground stop result could not be persisted");
                            }
                        }
                    }
                    // R4 前台契约：run 形态消费 Stop 并确认终态后，前台
                    // 进程以停止语义退出（restart_on_exit=false 传播 0），
                    // 不进入 serve 式的空闲驻留——用户的前台 run 不因远端
                    // 停止而永久挂起。
                    if foreground {
                        tracing::info!("foreground run consumed an admitted stop; exiting");
                        return Ok(());
                    }
                }
                Err(error) => {
                    tracing::error!("runtime stop business failed: {error:#}");
                    state.set_phase(ServerPhase::Failed(format!("stop: {error:#}")));
                    if let Err(settle_error) = state
                        .finish_runtime_operation_by_id(
                            &operation_id,
                            shared_types::RuntimeOperationState::RecoveryRequired,
                            Some((
                                shared_types::ERR_BACKEND_ERROR.into(),
                                format!("stop business unconfirmed: {error:#}"),
                            )),
                        )
                        .await
                    {
                        tracing::error!("{settle_error}; recovery hold engaged");
                    }
                }
            }
            continue;
        }
        let deployment_attempt = matches!(
            action,
            InitialAction::Deploy(_)
                | InitialAction::Prepared(_)
                | InitialAction::Source
                | InitialAction::SourcePrepared(_)
        );
        let mut prepared_proxy = None;
        let prepared = match action {
            InitialAction::StopBusiness { .. } | InitialAction::Settled => {
                unreachable!("handled above")
            }
            InitialAction::Deploy(request) => {
                if let Some(id) = &request.runtime_operation_id
                    && signal_operation_already_settled(state, id).await
                {
                    // RV01：同 next_prepared——旧会话残留信号不执行。
                    if !matches!(state.phase(), ServerPhase::Failed(_)) {
                        state.set_phase(ServerPhase::Idle);
                    }
                    continue;
                }
                if let Some(id) = &request.runtime_operation_id {
                    state.set_current_runtime_operation(Some(id.clone()));
                }
                if let Err(error) = state.record_runtime_deployment(&request) {
                    hold_unconfirmed(state, format!("persist deployment execution: {error:#}"))
                        .await;
                    continue;
                }
                let workspace = match resolved_execution_workspace(
                    &owner_args.workspace,
                    request.execution_target,
                    state,
                ) {
                    Ok(workspace) => workspace,
                    Err(error) => {
                        fail_preparation(state, format!("resolve deployment workspace: {error:#}"))
                            .await;
                        continue;
                    }
                };
                let target = request.execution_target;
                let pg = request.run_pg.clone();
                state.set_phase(ServerPhase::Deploying);
                state.set_request_release_id(&request.release_id);
                let state_ref = state.clone();
                let progress_cb: crate::deploy::ProgressCallback =
                    std::sync::Arc::new(move |p: shared_types::AppDeploymentProgress| {
                        state_ref.set_deploy_progress(p);
                    });
                match state
                    .preparations
                    .run(workspace.clone(), request, Some(progress_cb))
                    .await
                {
                    Ok(Some(prepared)) => Some(PreparedActivation {
                        prepared,
                        workspace,
                        target,
                        pg,
                        proxy: None,
                    }),
                    Ok(None) => {
                        // A release/hash cache hit skips file activation, not
                        // this operation's runtime configuration.
                        args.workspace = workspace;
                        state.set_pending_dev_profile(target == Some(ExecutionTarget::Source));
                        state.set_pending_run_config(pg);
                        None
                    }
                    Err(error) => {
                        fail_preparation(state, format!("prepare: {error:#}")).await;
                        continue;
                    }
                }
            }
            InitialAction::Prepared(prepared) => Some(prepared),
            InitialAction::Source => {
                args.workspace = match resolved_execution_workspace(
                    &owner_args.workspace,
                    Some(ExecutionTarget::Source),
                    state,
                ) {
                    Ok(workspace) => workspace,
                    Err(error) => {
                        fail_preparation(state, format!("resolve source workspace: {error:#}"))
                            .await;
                        continue;
                    }
                };
                if let Err(error) = crate::manifest::read_release_lock(&args.workspace)
                    .and_then(|release| state.record_prepared_artifact(release.release_id))
                {
                    fail_preparation(state, format!("record source identity: {error:#}")).await;
                    continue;
                }
                // Old services are already confirmed stopped before this action.
                if let Err(error) = state.persist_boundary(Boundary::Switching) {
                    hold_unconfirmed(state, format!("persist source switch: {error:#}")).await;
                    continue;
                }
                None
            }
            InitialAction::SourcePrepared(proxy) => {
                args.workspace = proxy.workspace.clone();
                state.record_prepared_artifact(proxy.release.release_id.clone())?;
                state.persist_boundary(Boundary::Switching)?;
                prepared_proxy = Some(proxy);
                None
            }
            InitialAction::ExistingPrepared(proxy) => {
                args.workspace = proxy.workspace.clone();
                prepared_proxy = Some(proxy);
                None
            }
            InitialAction::Existing { workspace } => {
                args.workspace = workspace;
                None
            }
        };
        if state.is_cancelled() {
            return Ok(());
        }
        if state.current_operation_cancelled() {
            drop(prepared);
            if let Err(error) = state.fail_operation(
                "deployment cancelled before activation".into(),
                Boundary::Preparing,
            ) {
                hold_unconfirmed(state, format!("persist cancellation: {error:#}")).await;
                continue;
            }
            if let Some(id) = state.current_runtime_operation() {
                state.settle_cancelled_before_execution(&id).await;
            }
            continue;
        }
        if let Some(prepared) = prepared.as_ref()
            && let Err(error) = prepared
                .prepared
                .artifact_release_id()
                .and_then(|id| state.record_prepared_artifact(id))
        {
            pending =
                fail_activation(args, state, format!("record prepared identity: {error:#}")).await;
            continue;
        }
        if prepared.is_some()
            && let Err(error) = state.persist_boundary(Boundary::Switching)
        {
            pending = fail_activation(args, state, format!("persist switch: {error:#}")).await;
            continue;
        }
        if let Some(prepared) = prepared {
            // Preparation binds the target before shutdown. A late request cannot
            // redirect this activation or inject its credentials into this epoch.
            args.workspace = prepared.workspace;
            state.set_pending_dev_profile(prepared.target == Some(ExecutionTarget::Source));
            state.set_pending_run_config(prepared.pg);
            prepared_proxy = prepared.proxy;
            if let Err(error) = crate::deploy::activate(&args.workspace, prepared.prepared).await {
                pending = fail_activation(args, state, format!("activate: {error:#}")).await;
                continue;
            }
        }

        // ── Orchestrating：读 lock → 编排（migrate → services → pingap → readiness）──
        match prepared_proxy
            .as_ref()
            .map(|proxy| Ok(proxy.release.clone()))
            .unwrap_or_else(|| crate::manifest::read_release_lock(&args.workspace))
        {
            Ok(release) => {
                state.set_release(release);
                if deployment_attempt && let Err(error) = state.complete_stage() {
                    pending =
                        fail_activation(args, state, format!("persist activation: {error:#}"))
                            .await;
                    continue;
                }
                state.set_phase(ServerPhase::Orchestrating);
            }
            Err(e) => {
                tracing::error!("server: read release lock after deploy: {e:#}");
                pending = fail_activation(args, state, format!("release lock: {e:#}")).await;
                continue;
            }
        }

        // ── 引擎分派：supervisord 托管（编排完成即返回，服务由 supervisord
        // per-service 重启）与 builtin（编排+supervise 阻塞在同一 task）──
        if state.is_cancelled() {
            return Ok(());
        }
        // R08：本次操作的 dev profile（Source 形态编排显式传递；未指定 =
        // legacy/部署路径 → env 兜底）。take 一次性消费——每次编排对应一次取值
        let run_dev_profile = state
            .take_pending_dev_profile()
            .unwrap_or_else(crate::supervisor::dev_run_profile);
        state.set_proxy_context(args.workspace.clone(), run_dev_profile);
        // R08：本次操作的 PG 凭据（owner 复用时平台传入的新凭据——注入服务
        // env 覆盖旧值）。take 一次性消费；未携带 → None（维持进程 env）
        let run_pg = state.take_pending_run_config();
        let mut hot_rx = state.deploy_rx.lock().await;
        if let Some(host) = &host {
            let runtime_status = state.runtime_status();
            let Some(release) = state.release() else {
                state.set_phase(ServerPhase::Failed(
                    "orchestration release is missing".into(),
                ));
                continue;
            };
            let orchestration_cancel = state.cancel_child_token();
            let orchestration = host.orchestrate(
                args,
                &release,
                &runtime_status,
                crate::supervisor::RunProfile {
                    run_migrations,
                    dev_profile: run_dev_profile,
                    pg: run_pg,
                    prepared_proxy: prepared_proxy.clone(),
                },
                &orchestration_cancel,
            );
            tokio::pin!(orchestration);
            let mut startup_control = state.control_rx.lock().await;
            let mut startup_signal = None;
            let outcome = tokio::select! {
                result = &mut orchestration => result,
                signal = startup_control.recv() => {
                    startup_signal = signal;
                    orchestration_cancel.cancel();
                    (&mut orchestration).await
                }
                () = async { state.cancel_token().cancelled().await } => {
                    orchestration_cancel.cancel();
                    (&mut orchestration).await
                }
            };
            drop(startup_control);
            if let Some(signal) = startup_signal {
                if let Err(error) = host.stop_all(Some(&standby_root)).await {
                    record_uncertain_control(state, &signal, &format!("startup stop: {error:#}"))
                        .await;
                    hold_unconfirmed(state, format!("startup stop: {error:#}")).await;
                    return Ok(());
                }
                if let Err(error) = &outcome
                    && error
                        .downcast_ref::<supervisor::ShutdownUnconfirmed>()
                        .is_some()
                {
                    record_uncertain_control(
                        state,
                        &signal,
                        &format!("startup outcome: {error:#}"),
                    )
                    .await;
                    hold_unconfirmed(state, format!("startup outcome: {error:#}")).await;
                    return Ok(());
                }
                let reason = outcome
                    .err()
                    .map(|error| format!("startup interrupted: {error:#}"))
                    .unwrap_or_else(|| "startup interrupted by runtime control".into());
                pending = Some(settle_stopped_startup(args, state, signal, reason).await);
                continue;
            }
            if state.is_cancelled() {
                host.finish_business_session(Some(&standby_root)).await?;
                if let Err(error) = outcome
                    && error
                        .downcast_ref::<supervisor::ShutdownUnconfirmed>()
                        .is_some()
                {
                    state.begin_failure(format!("shutdown RPC outcome: {error:#}"), true);
                    return Err(error);
                }
                return Ok(());
            }
            let mut preflight_rejected = false;
            if let Err(e) = outcome {
                if e.downcast_ref::<supervisor::PreflightRejected>().is_some() {
                    fail_preparation(state, format!("proxy preflight: {e:#}")).await;
                    state.set_phase(ServerPhase::Running);
                    preflight_rejected = true;
                } else {
                    tracing::error!("server: orchestration failed: {e:#}");
                    if let Err(stop_error) = host.finish_business_session(Some(&standby_root)).await
                    {
                        hold_unconfirmed(
                            state,
                            format!("orchestrate: {e:#}; stop: {stop_error:#}"),
                        )
                        .await;
                        return Ok(());
                    }
                    if e.downcast_ref::<supervisor::ShutdownUnconfirmed>()
                        .is_some()
                    {
                        hold_unconfirmed(state, format!("orchestrate: {e:#}")).await;
                        return Ok(());
                    }
                    pending = fail_activation(args, state, format!("orchestrate: {e:#}")).await;
                    continue;
                }
            }
            if !preflight_rejected && let Err(error) = state.complete_running() {
                if let Err(stop_error) = host.stop_all(Some(&standby_root)).await {
                    hold_unconfirmed(
                        state,
                        format!("persist running: {error:#}; stop: {stop_error:#}"),
                    )
                    .await;
                    return Ok(());
                }
                pending = fail_activation(args, state, format!("persist running: {error:#}")).await;
                continue;
            }
            // V02：取消观察并入提交屏障（内核锁内检查，消除外层检查与提交
            // 之间的竞争窗口）——Cancelled/Superseded 都必须**先停服确认**
            // 再按 ID 收束 Cancelled；清理未知走 hold_unconfirmed。
            match if preflight_rejected {
                BarrierOutcome::Passed
            } else {
                commit_running_barrier(state).await
            } {
                BarrierOutcome::Passed => {}
                BarrierOutcome::CancelledByRequest | BarrierOutcome::Superseded => {
                    let reason = if commit_running_barrier_reason_cancelled(state) {
                        "cancelled during orchestration"
                    } else {
                        "superseded by an admitted stop operation"
                    };
                    if let Err(error) = host.stop_all(Some(&standby_root)).await {
                        hold_unconfirmed(
                            state,
                            format!("stop after cancelled/superseded startup failed: {error:#}"),
                        )
                        .await;
                        return Ok(());
                    }
                    if let Some(operation_id) = state.current_runtime_operation()
                        && let Err(settle_error) = state
                            .finish_runtime_operation_by_id(
                                &operation_id,
                                shared_types::RuntimeOperationState::Cancelled,
                                Some((shared_types::ERR_RECOVERY_REQUIRED.into(), reason.into())),
                            )
                            .await
                    {
                        tracing::error!("{settle_error}; recovery hold engaged");
                        state.set_phase(ServerPhase::Failed(settle_error));
                        continue;
                    }
                    state.set_phase(ServerPhase::Idle);
                    continue;
                }
            }
            // R01：supervisord Running 等待也消费运行控制信号（Stop/重启编排
            // 不再只能等 Idle）。锁序与 Idle 分支一致：deploy 先、control 后。
            pending =
                wait_supervisord_running(owner_args, args, state, host, &mut hot_rx, &standby_root)
                    .await?;
            if pending.is_none() {
                return Ok(());
            }
            continue;
        }

        // builtin：编排 supervise（可被下一次部署请求打断：cancel → 停服 → 回
        // Deploying）；编排完成进 supervise 时经 on_running 通知 → 相位切 Running。
        let cancel = state.cancel_child_token();
        let runtime_status = state.runtime_status();
        let (running_tx, mut running_rx) = tokio::sync::oneshot::channel::<()>();
        // RV05：内置引擎的服务编排任务延续捕获的命令范围——会话结束后
        // 迟到的受管命令仍向旧（已闭门）范围登记，不进入 Direct/新代次。
        let mut sup = process_utils::command_context::spawn_scoped(supervisor::run_with_cancel(
            args.clone(),
            runtime_status,
            cancel.clone(),
            Some(running_tx),
            supervisor::RunProfile {
                run_migrations,
                dev_profile: run_dev_profile,
                pg: run_pg,
                prepared_proxy,
            },
        ));
        let mut sup_joined = false;
        // Startup must consume Stop too; waiting only for readiness can leave
        // an admitted Stop blocked behind a failing service indefinitely.
        let mut startup_control = state.control_rx.lock().await;
        let next = tokio::select! {
            result = &mut running_rx => {
                drop(startup_control);
                if result.is_err() {
                    // The supervisor exited before readiness. Consume its
                    // outcome before reading the next request: otherwise a
                    // queued Start can win the select and a confirmed startup
                    // failure is mistaken for an unconfirmed stop of that Start.
                    match join_supervisor(&mut sup, &mut sup_joined).await {
                        Err(error) if error.downcast_ref::<supervisor::ShutdownUnconfirmed>().is_some()
                            || error.downcast_ref::<tokio::task::JoinError>().is_some() => {
                            hold_unconfirmed(state, format!("startup shutdown unconfirmed: {error:#}")).await;
                            return Ok(());
                        }
                        Err(error) => {
                            pending = fail_activation(args, state, format!("orchestrate: {error:#}")).await;
                        }
                        Ok(()) => {
                            pending = fail_activation(args, state, "supervisor ended before readiness".into()).await;
                        }
                    }
                    continue;
                }
                if result.is_ok() && let Err(error) = state.complete_running() {
                        cancel.cancel();
                        if join_supervisor(&mut sup, &mut sup_joined).await.is_err() {
                            hold_unconfirmed(state, format!("persist running failed and shutdown unconfirmed: {error:#}")).await;
                            return Ok(());
                        }
                        pending = fail_activation(args, state, format!("persist running: {error:#}")).await;
                        continue;
                }
                if result.is_ok() {
                    // V02：取消观察并入提交屏障（内核锁内检查，消除外层检查与
                    // 提交之间的窗口）；Cancelled/Superseded 统一"先停本组服务
                    // 确认，再按 ID 收束 Cancelled"，清理未知走 hold_unconfirmed。
                    // B02：join 后 JoinHandle 不得再 poll。
                    let barrier = commit_running_barrier(state).await;
                    if barrier != BarrierOutcome::Passed {
                        let reason = if barrier == BarrierOutcome::CancelledByRequest {
                            "cancelled during orchestration"
                        } else {
                            "superseded by an admitted stop operation"
                        };
                        cancel.cancel();
                        if let Err(error) = join_supervisor(&mut sup, &mut sup_joined).await {
                            hold_unconfirmed(
                                state,
                                format!("stop after cancelled/superseded startup failed: {error:#}"),
                            )
                            .await;
                            return Ok(());
                        }
                        if let Some(operation_id) = state.current_runtime_operation()
                            && let Err(settle_error) = state
                                .finish_runtime_operation_by_id(
                                    &operation_id,
                                    shared_types::RuntimeOperationState::Cancelled,
                                    Some((shared_types::ERR_RECOVERY_REQUIRED.into(), reason.into())),
                                )
                                .await
                            {
                                tracing::error!("{settle_error}; recovery hold engaged");
                                state.set_phase(ServerPhase::Failed(settle_error));
                                continue;
                            }
                        state.set_phase(ServerPhase::Idle);
                        continue;
                    }
                }
                let mut builtin_control = state.control_rx.lock().await;
                loop {
                let next = tokio::select! {
                    outcome = &mut sup, if !sup_joined => { sup_joined = true; match outcome {
                        // 服务退出/信号/cancel 后 supervise 正常返回：内置引擎不自动重编排
                //（supervisord 引擎下服务崩溃由 supervisord per-service 重启，不走到这）
                Ok(Ok(())) => {
                    tracing::warn!("server: orchestration ended (service exit or signal)");
                    Next::Wait
                }
                Ok(Err(e)) => {
                    tracing::error!("server: orchestration failed: {e:#}");
                    if e.downcast_ref::<supervisor::ShutdownUnconfirmed>().is_some() {
                        hold_unconfirmed(state, format!("orchestrate: {e:#}")).await;
                        return Ok(());
                    }
                    pending = fail_activation(args, state, format!("orchestrate: {e:#}")).await;
                    Next::Wait
                }
                Err(join) => {
                    tracing::error!("server: orchestration task panicked: {join}");
                    hold_unconfirmed(state, format!("orchestrate panicked: {join}")).await;
                    return Ok(());
                }
            }},
            maybe = next_prepared(owner_args, &args.workspace, state, &mut hot_rx) => match maybe {
                Some(action) => {
                    let predecessor_ready = state.ready.is_ready();
                    if let Err(error) = supervisor::resident::publish_standby_if_serving().await {
                        reject_live_deployment(state, error, predecessor_ready).await;
                        continue;
                    }
                    state.ready.set_ready(false);
                    cancel.cancel();
                    if let Err(error) = join_supervisor(&mut sup, &mut sup_joined).await {
                        hold_unconfirmed(state, format!("stop before activation failed: {error:#}")).await;
                        return Ok(());
                    }
                    Next::Redeploy(Box::new(action))
                }
                None => Next::Exit,
            },
            // R01：builtin Running 等待也消费运行控制信号（先停本组服务再收束）
            signal = builtin_control.recv() => match signal {
                Some(signal) => {
                    if !claim_live_source_control(state, &signal).await { continue; }
                    let prepared = match prepare_source_control(owner_args, state, &signal).await {
                        Ok(prepared) => prepared,
                        Err(error) => { reject_live_control(state, &signal, &error).await; continue; }
                    };
                    if source_cancelled_after_preflight(state, &signal).await { continue; }
                    if let Err(error) = supervisor::resident::publish_standby_if_serving().await {
                        reject_live_control(state, &signal, &error).await;
                        continue;
                    }
                    let action = settled_prepared_control(state, signal, prepared).await;
                    if matches!(action, InitialAction::Settled) { continue; }
                    state.ready.set_ready(false);
                    cancel.cancel();
                    if let Err(error) = join_supervisor(&mut sup, &mut sup_joined).await {
                        hold_unconfirmed(state, format!("stop before runtime control failed: {error:#}")).await;
                        return Ok(());
                    }
                    Next::Redeploy(Box::new(action))
                }
                None => Next::Wait,
            },
                    () = async { state.cancel_token().cancelled().await } => {
                        cancel.cancel();
                        join_supervisor(&mut sup, &mut sup_joined).await?;
                        Next::Exit
                    }
                };
                break next;
                }
            }
            signal = startup_control.recv() => {
                drop(startup_control);
                state.ready.set_ready(false);
                cancel.cancel();
                let stopped = join_supervisor(&mut sup, &mut sup_joined).await;
                if let Err(error) = &stopped
                    && (error.downcast_ref::<supervisor::ShutdownUnconfirmed>().is_some()
                        || error.downcast_ref::<tokio::task::JoinError>().is_some())
                {
                    if let Some(signal) = &signal {
                        record_uncertain_control(state, signal, &format!("startup stop unconfirmed: {error:#}")).await;
                    }
                    hold_unconfirmed(state, format!("startup stop unconfirmed: {error:#}")).await;
                    return Ok(());
                }
                // A normal startup error has already reaped its children. Keep
                // that diagnostic, but never complete the newly received Stop
                // using the old startup's identity.
                let reason = stopped.err().map(|error| format!("startup interrupted: {error:#}"))
                    .unwrap_or_else(|| "startup interrupted by runtime control".into());
                match signal {
                    Some(signal) => Next::Redeploy(Box::new(settle_stopped_startup(args, state, signal, reason).await)),
                    None => Next::Exit,
                }
            },
            () = async { state.cancel_token().cancelled().await } => {
                cancel.cancel();
                join_supervisor(&mut sup, &mut sup_joined).await?;
                Next::Exit
            },
        };
        match next {
            Next::Exit => {
                cancel.cancel();
                join_supervisor(&mut sup, &mut sup_joined).await?;
                return Ok(());
            }
            Next::Wait => {}
            Next::Redeploy(action) => pending = Some(*action),
        }
    }
}

/// EVT 行 JSON → journal 事件字段（R06 事件桥）。`orchestration_done` 的
/// failed 清单与 `service_start_fail` 的 error 放 payload——消费侧按事件流
/// 即可还原终局与失败明细，不需要再读 stdout。
pub(super) struct BridgeEvent {
    pub(super) stage: String,
    pub(super) service: Option<String>,
    pub(super) event_name: String,
    pub(super) payload: Option<serde_json::Value>,
}

pub(super) fn bridge_event_fields(json: &str) -> Option<BridgeEvent> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let event_name = value.get("event")?.as_str()?.to_string();
    let service = value
        .get("service")
        .and_then(|service| service.as_str())
        .map(str::to_string);
    let stage = match event_name.as_str() {
        "orchestration_done" => "orchestration",
        "log" => "migration",
        _ => "service",
    };
    let payload = match event_name.as_str() {
        "log" => value
            .get("line")
            .cloned()
            .map(|line| serde_json::json!({ "line": line })),
        "orchestration_done" => value
            .get("failed")
            .cloned()
            .map(|failed| serde_json::json!({ "failed": failed })),
        "service_start_fail" => value
            .get("error")
            .cloned()
            .map(|error| serde_json::json!({ "error": error })),
        _ => None,
    };
    Some(BridgeEvent {
        stage: stage.to_string(),
        service,
        event_name,
        payload,
    })
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::io::{Read, Write};

    // A real child process supplies the predecessor HTTP/PID evidence. The
    // supervisor control socket is a protocol fixture, not deployment evidence.
    #[test]
    #[ignore = "subprocess fixture, launched by repeated_source_rejection_keeps_running_driver_and_http"]
    fn http_service_fixture() {
        let Some(path) = std::env::var_os("APP_CLI_P1_HTTP_FIXTURE") else {
            return;
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std::fs::write(path, listener.local_addr().unwrap().to_string()).unwrap();
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = [0; 4096];
            if stream.read(&mut request).unwrap() == 0 {
                continue;
            }
            let body = format!("old-pid={}", std::process::id());
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .unwrap();
        }
    }

    fn custom_release(id: &str) -> ReleaseLock {
        toml::from_str(&format!(
            r#"
            schema_version = 1
            release_id = "{id}"
            workspace_name = "fixture"
            minimum_app_cli_version = "0.1.3"
            runtime_image_digest = "fixture"
            [pingap]
            mode = "custom"
            config = "proxy.toml"
            version = "0.15.0"
            commit = "fixture"
            [[services]]
            service_id = "web"
            name = "Web"
            dir = "web"
            type = "node"
            kind = "web"
            enabled = true
            port = 4200
            logs = []
            [services.run]
            command = ["node", "server.js"]
            migrate = []
            depends_on = []
            shutdown_timeout_seconds = 3
            [services.health]
            startup_path = "/health"
            readiness_path = "/ready"
            liveness_path = "/health"
            [services.env]
        "#
        ))
        .unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repeated_source_rejection_keeps_running_driver_and_http() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("workspace");
        let current = source.join(".run");
        std::fs::create_dir_all(&current).unwrap();
        let release = custom_release("source-b");
        std::fs::write(
            source.join("release.lock.toml"),
            toml::to_string(&release).unwrap(),
        )
        .unwrap();
        std::fs::write(
            current.join("release.lock.toml"),
            toml::to_string(&custom_release("artifact-a")).unwrap(),
        )
        .unwrap();
        let config = |port| format!("[servers.app]\naddr = \"127.0.0.1:{port}\"\n");
        std::fs::write(source.join("proxy.toml"), config(19082)).unwrap();
        std::fs::write(current.join("proxy.toml"), config(19081)).unwrap();
        let log_dir = root.path().join("logs");
        let runtime_root = crate::proxy::compiler::runtime_root(&log_dir);
        let active = crate::proxy::compiler::active_config_path(&runtime_root);
        std::fs::create_dir_all(active.parent().unwrap()).unwrap();
        std::fs::write(&active, config(19081)).unwrap();
        let marker = root.path().join("http-address");
        let mut business = tokio::process::Command::new(std::env::current_exe().unwrap())
            .arg("--ignored")
            .arg("--exact")
            .arg("orchestration::server::run_loop::lifecycle_tests::http_service_fixture")
            .arg("--nocapture")
            .env("APP_CLI_P1_HTTP_FIXTURE", &marker)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = business.id().unwrap();
        let address = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Ok(address) = std::fs::read_to_string(&marker) {
                    break address;
                }
                assert!(
                    business.try_wait().unwrap().is_none(),
                    "HTTP fixture exited before listen"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let pingap = root.path().join("pingap");
        std::fs::write(
            &pingap,
            "#!/bin/sh\nif [ \"$1\" = '--apply-protocol-version' ]; then echo 1; fi\nexit 0\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pingap, std::fs::Permissions::from_mode(0o755)).unwrap();
        let owner_args = RuntimeArgs {
            workspace: source.clone(),
            log_dir,
            pingap_bin: pingap,
            ..Default::default()
        };
        let mut active_args = owner_args.clone();
        active_args.workspace = current.clone();
        let state = Arc::new(ServerState::new(RuntimeStatusService::default()));
        state.set_phase(ServerPhase::Running);
        state.ready.set_ready(true);
        let kernel = Arc::new(crate::runtime_kernel::RuntimeKernel::new(
            crate::runtime_kernel::RuntimeStore::open_with_root(root.path().join("state"), &source)
                .unwrap(),
            shared_types::RuntimeIdentityView {
                application_id: "p1app".into(),
                workspace_id: "p1ws".into(),
                service_family: "userapp-dev".into(),
                source_root: source.to_string_lossy().into_owned(),
                runtime_instance_id: "instance".into(),
                deployment_generation_id: "generation".into(),
                protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
                capabilities: vec![],
            },
            Box::new(|_| {}),
        ));
        state.set_runtime_kernel(kernel.clone());
        let host = SupervisordHost::protocol_fixture(
            root.path().join("unavailable-supervisor.sock"),
            root.path().join("dynamic.conf"),
        );
        let (_hot, mut hot_rx) = tokio::sync::mpsc::unbounded_channel();
        let driver_state = state.clone();
        let driver_args = owner_args.clone();
        let task = tokio::spawn(async move {
            wait_supervisord_running(
                &driver_args,
                &active_args,
                &driver_state,
                &host,
                &mut hot_rx,
                &runtime_root,
            )
            .await
        });
        for id in ["rejectone", "rejecttwo"] {
            let revision = kernel.store().load_desired().unwrap().1;
            kernel
                .admit(shared_types::RuntimeOperationRequest {
                    operation_id: id.into(),
                    expected_runtime_instance_id: "instance".into(),
                    expected_revision: revision,
                    workspace_id: "p1ws".into(),
                    kind: shared_types::RuntimeOperationKind::Start,
                    profile: shared_types::RunProfileInput::Source {
                        workspace_id: "p1ws".into(),
                    },
                    run_config: None,
                    request_context: None,
                })
                .await
                .unwrap();
            state
                .control_tx
                .send(ControlSignal::OrchestrateSource {
                    operation_id: id.into(),
                    dev_profile: true,
                    pg: None,
                })
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(4), async {
                loop {
                    if kernel.get(id).await.unwrap().unwrap().state
                        == shared_types::RuntimeOperationState::Failed
                    {
                        break;
                    }
                    assert!(
                        !task.is_finished(),
                        "rejection must keep the Running driver alive"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(
                kernel
                    .get(id)
                    .await
                    .unwrap()
                    .unwrap()
                    .error_message
                    .as_deref()
                    .is_some_and(|message| message.contains("server")),
                "the source listener change must be the rejection reason"
            );
            assert_eq!(state.phase(), ServerPhase::Running);
            assert!(state.ready.is_ready());
            assert_eq!(std::fs::read_to_string(&active).unwrap(), config(19081));
            assert!(business.try_wait().unwrap().is_none());
            assert_eq!(business.id(), Some(pid));
            let mut stream = tokio::net::TcpStream::connect(&address).await.unwrap();
            stream
                .write_all(b"GET / HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 200"));
            assert!(response.ends_with(&format!("old-pid={pid}")));
        }
        task.abort();
        let _ = task.await;
        business.kill().await.unwrap();
        business.wait().await.unwrap();
    }
}
