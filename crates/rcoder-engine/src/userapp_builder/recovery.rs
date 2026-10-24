//! Restart discovery. Automatic replay is limited to unclaimed, matching inputs.
mod compute;
mod discovery;
#[cfg(test)]
mod hot_tests;
#[cfg(test)]
mod predecessor_timeout_tests;
use crate::app_state::AppState;
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture, stream::FuturesUnordered};
use shared_types::{AppWakeControl as _, UserAppOperationKind, UserAppOperationState};
use std::{
    collections::HashSet, future::Future, panic::AssertUnwindSafe, sync::Weak, time::Duration,
};

const MAX_RECOVERIES: usize = 8;
const SCAN_PAGE_SIZE: u32 = 128;
const MAX_PAGES_PER_TICK: usize = 4;
const SCAN_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// 恢复任务对底层 runtime 调用的统一卡死预算。kube 请求无总超时
/// （kube-rs 默认 read_timeout=None），一个 stall 调用不加界会把恢复任务
/// 永久挂死——占满 `MAX_RECOVERIES` 槽位后瘫痪整个恢复管线。
const STALL_BUDGET: Duration = Duration::from_secs(60);

/// 以 [`STALL_BUDGET`] 为界执行恢复期的 runtime 调用；超时统一按
/// `{label} timed out (stalled runtime call)` 传播错误，由下一扫描周期
/// 重试（错误不吞、任务收束、槽位释放）。
async fn with_stall_budget<F: Future>(label: &str, fut: F) -> anyhow::Result<F::Output> {
    tokio::time::timeout(STALL_BUDGET, fut)
        .await
        .map_err(|_| anyhow::anyhow!("{label} timed out (stalled runtime call)"))
}

async fn sweep_legacy_locks(
    runtime: &dyn container_runtime_api::UserAppDeploymentRuntime,
) -> anyhow::Result<Vec<String>> {
    with_stall_budget(
        "Sweep legacy operation locks",
        runtime.sweep_legacy_operation_locks(Duration::from_secs(24 * 3600)),
    )
    .await?
    .map_err(anyhow::Error::from)
}

type RecoveryResult = (String, anyhow::Result<()>);

/// Bounded process-local scheduling only. Durable claims and resource leases
/// remain authoritative; occupying a slot does not authorize a resource write.
#[derive(Default)]
struct RecoveryTasks {
    active: HashSet<String>,
    pending: FuturesUnordered<BoxFuture<'static, RecoveryResult>>,
}

#[cfg(test)]
pub(super) async fn run_compute_scan_for_test(
    state: &std::sync::Arc<AppState>,
) -> anyhow::Result<()> {
    let mut tasks = RecoveryTasks::default();
    compute::discover_pending(state, &mut tasks, &mut None).await?;
    while let Some((_, result)) = tasks.next().await {
        result?;
    }
    Ok(())
}

impl RecoveryTasks {
    fn is_full(&self) -> bool {
        self.active.len() >= MAX_RECOVERIES
    }

    /// 槽满 = 扫描器瘫痪面（本次事故 9 小时不可见的直接成因：挂死任务
    /// 占满后所有 discover 静默 return，零日志零收束）。返回 true 时打
    /// warn 并带上挂死任务清单——瘫痪必须可见。
    fn reject_if_full(&self, gate: &str) -> bool {
        if !self.is_full() {
            return false;
        }
        tracing::warn!(
            gate,
            active = ?self.active.iter().cloned().collect::<Vec<_>>(),
            "Recovery scanner saturated: tasks are not completing (likely a stalled runtime call without timeout); discovery is paused"
        );
        true
    }

    fn push(
        &mut self,
        operation_id: String,
        task: impl Future<Output = anyhow::Result<()>> + Send + 'static,
    ) -> bool {
        if self.is_full() || !self.active.insert(operation_id.clone()) {
            return false;
        }
        self.pending.push(
            async move {
                // A panic must not terminate discovery or discard other in-flight
                // recovery futures. Resource ownership is still retained by their guards.
                let result = AssertUnwindSafe(task)
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!(
                            "UserApp recovery task panicked; inspect durable operation state"
                        ))
                    });
                (operation_id, result)
            }
            .boxed(),
        );
        true
    }

    async fn next(&mut self) -> Option<RecoveryResult> {
        let result = self.pending.next().await?;
        self.active.remove(&result.0);
        Some(result)
    }
}

/// 启动恢复扫描器；返回任务句柄供关机协调者等待退出（R02）。
/// `shutdown_rx` 触发后停止发现新工作，等待在途恢复任务收束。
/// 总关机预算由调用者统一控制；扫描器不能提前返回“已排空”。
pub(crate) fn start_recovery(
    state: Weak<AppState>,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut tasks = RecoveryTasks::default();
        let mut cursor = None;
        let mut terminal_cursor = None;
        let mut compute_cursor = None;
        let mut pending_compute_cursor = None;
        let mut terminal_first = false;
        let mut sweep_ticks: u32 = 0;
        let mut discovery = discovery::Discovery::default();
        let reason = loop {
            tokio::select! {
                result = tasks.next(), if !tasks.active.is_empty() => {
                    if let Some((operation_id, Err(error))) = result {
                        tracing::warn!(%error, %operation_id, "Pending userApp operation was not resumed");
                    }
                }
                _ = shutdown_rx.recv() => break "shutdown",
                _ = interval.tick() => {
                    let Some(state) = state.upgrade() else { return; };
                    terminal_first = !terminal_first;

                    if let Err(error) = compute::discover_pending(&state, &mut tasks, &mut pending_compute_cursor).await {
                        tracing::error!(%error, "Pending compute scan failed");
                    }
                    if let Err(error) = compute::discover_compute_leases(state.userapp_store.clone(), state.runtime.clone(), &mut tasks, &mut compute_cursor).await {
                        tracing::error!(%error, "Compute control lease scan failed");
                    }
                    // Receipt listing touches cluster objects or the receipt
                    // directory; run it on a slow cadence instead of per tick.
                    sweep_ticks = sweep_ticks.wrapping_add(1);
                    if sweep_ticks.is_multiple_of(72)
                        && let Err(error) = compute::sweep_builder_creation_receipts(state.userapp_store.clone(), state.runtime.clone(), &mut tasks).await {
                        tracing::warn!(%error, "Builder creation receipt sweep failed");
                    }
                    // 迁移清扫：Lease 化之后 acquire 不再产生 ConfigMap 锁，
                    // 残存的 rcoder-operation-* ConfigMap 是迁移前存量/孤儿——
                    // 24h 龄期（覆盖滚动升级窗口）+ uid precondition 删除。
                    // 只释放陈旧互斥，不授权任何变更。
                    if sweep_ticks.is_multiple_of(720) {
                        match sweep_legacy_locks(state.runtime.as_ref()).await {
                            Ok(names) if !names.is_empty() => {
                                tracing::info!(count = names.len(), ?names,
                                    "Legacy ConfigMap operation locks swept");
                            }
                            Ok(_) => {}
                            Err(error) => {
                                tracing::warn!(%error, "Legacy operation lock sweep failed");
                            }
                        }
                    }
                    if terminal_first
                        && let Err(error) = discover_terminal_leases(state.userapp_store.clone(), state.runtime.clone(), &mut tasks, &mut terminal_cursor).await {
                        tracing::error!(%error, "UserApp terminal lease scan failed");
                    }
                    if let Err(error) = discover(&state, &mut tasks, &mut cursor).await {
                        tracing::error!(%error, "UserApp pending operation scan failed");
                    }
                    if !terminal_first
                        && let Err(error) = discover_terminal_leases(state.userapp_store.clone(), state.runtime.clone(), &mut tasks, &mut terminal_cursor).await {
                        tracing::error!(%error, "UserApp terminal lease scan failed");
                    }
                    if let Err(error) = discovery.poll(&state, &mut tasks).await {
                        tracing::warn!(%error, "Managed lifecycle discovery requires inspection");
                    }
                }
            }
        };
        // The shutdown coordinator owns the deadline. Until every future has
        // settled, this handle remains pending and storage must remain open.
        while !tasks.active.is_empty() {
            if let Some((operation_id, Err(error))) = tasks.next().await {
                tracing::warn!(%error, %operation_id, "UserApp recovery task failed during drain");
            }
        }
        tracing::info!(reason, "UserApp recovery scanner drained and exited");
    })
}

/// Terminal operations are absent from unfinished_operations. Keep a separate
/// cursor so a crash between SQL commit and lease release cannot hide its mutex.
async fn discover_terminal_leases(
    store: std::sync::Arc<dyn shared_types::UserAppLifecycleStore>,
    runtime: std::sync::Arc<dyn container_runtime_api::UserAppDeploymentRuntime>,
    tasks: &mut RecoveryTasks,
    cursor: &mut Option<String>,
) -> anyhow::Result<()> {
    if tasks.reject_if_full("terminal_lease_scan") {
        return Ok(());
    }
    let query_cursor = cursor.clone();
    let read = tokio::time::timeout(
        SCAN_READ_TIMEOUT,
        store.scan_terminal_operation_leases(query_cursor.as_deref(), SCAN_PAGE_SIZE),
    );
    tokio::pin!(read);
    let page = loop {
        tokio::select! {
            result = &mut read => break result.map_err(|_| anyhow::anyhow!("Terminal lease storage scan timed out"))??,
            result = tasks.next(), if !tasks.active.is_empty() => {
                if let Some((operation_id, Err(error))) = result {
                    tracing::warn!(%error, %operation_id, "UserApp recovery task failed");
                }
            }
        }
    };
    if page.records.is_empty() {
        // A page of retained malformed rows is not end-of-scan. Advance by the
        // raw tail, leaving each bad row protected while other apps can recover.
        *cursor = if page.exhausted {
            None
        } else {
            page.next_after
        };
        return Ok(());
    }
    let raw_tail = page.next_after;
    let mut consumed_all = true;
    for binding in page.records {
        if tasks.reject_if_full("terminal_lease_push") {
            consumed_all = false;
            break;
        }
        *cursor = Some(binding.context.operation_id.clone());
        let store = store.clone();
        let runtime = runtime.clone();
        tasks.push(binding.context.operation_id.clone(), async move {
            with_stall_budget("Release terminal operation lease", async move {
                runtime
                    .release_app_operation_receipt(&binding.context, &binding.receipt)
                    .await?;
                store.forget_operation_lease(&binding).await?;
                Ok(())
            })
            .await?
        });
    }
    if consumed_all {
        *cursor = raw_tail;
    }
    Ok(())
}

async fn discover(
    state: &std::sync::Arc<AppState>,
    tasks: &mut RecoveryTasks,
    cursor: &mut Option<String>,
) -> anyhow::Result<()> {
    // Keep a cursor across ticks so blocked or large early pages cannot starve
    // later applications. Wrapping reconsiders earlier Pending operations.
    for _ in 0..MAX_PAGES_PER_TICK {
        if tasks.reject_if_full("discover_page") {
            return Ok(());
        }
        let query_cursor = cursor.clone();
        let read = tokio::time::timeout(
            SCAN_READ_TIMEOUT,
            state
                .userapp_store
                .scan_unfinished_operations(query_cursor.as_deref(), SCAN_PAGE_SIZE),
        );
        tokio::pin!(read);
        let page = loop {
            tokio::select! {
                result = &mut read => break result.map_err(|_| anyhow::anyhow!("UserApp recovery scan storage read timed out"))??,
                result = tasks.next(), if !tasks.active.is_empty() => {
                    if let Some((operation_id, Err(error))) = result {
                        tracing::warn!(%error, %operation_id, "Pending userApp operation was not resumed");
                    }
                }
            }
        };
        if page.records.is_empty() {
            *cursor = if page.exhausted {
                None
            } else {
                page.next_after
            };
            if page.exhausted {
                return Ok(());
            }
            continue;
        }
        let raw_tail = page.next_after;
        for operation in page.records {
            if tasks.reject_if_full("dispatch") {
                return Ok(());
            }
            *cursor = Some(operation.operation_id.clone());
            if operation.state == UserAppOperationState::RecoveryRequired
                && operation.kind == UserAppOperationKind::StartDeployment
                && operation.step == "creating_runtime"
            {
                let state = state.clone();
                tasks.push(operation.operation_id.clone(), async move {
                    if !state
                        .app_service
                        .reconcile_created_runtime_configuration(&operation)
                        .await?
                    {
                        with_stall_budget(
                            "observe interrupted production creation",
                            super::creation::reconcile_fenced_ensure(&state, &operation),
                        )
                        .await??;
                    }
                    Ok(())
                });
                continue;
            }
            let hot_recovery_state = match operation.state {
                UserAppOperationState::Running | UserAppOperationState::RecoveryRequired => true,
                UserAppOperationState::Pending
                | UserAppOperationState::WaitingRetry
                | UserAppOperationState::Succeeded
                | UserAppOperationState::Failed => false,
            };
            if hot_recovery_state
                && operation.kind.supports_hot_deployment()
                && matches!(operation.step.as_str(), "hot_execution" | "hot_converging")
                && operation
                    .checkpoint
                    .pointer("/hot_execution/receipt_protocol")
                    .and_then(serde_json::Value::as_u64)
                    == Some(1)
            {
                let state = state.clone();
                tasks.push(operation.operation_id.clone(), async move {
                    with_stall_budget(
                        "observe interrupted hot deployment",
                        state.app_service.resume_pending_control(&operation),
                    )
                    .await??;
                    Ok(())
                });
                continue;
            }
            if operation.kind == UserAppOperationKind::Start
                && operation.state == UserAppOperationState::Running
                && operation.step == "traffic_wake_observing"
            {
                let state = state.clone();
                tasks.push(operation.operation_id.clone(), async move {
                    with_stall_budget("recover expired traffic wake observation", async {
                        let Some(app) = state
                            .userapp_store
                            .get_application(&operation.app_id)
                            .await?
                        else {
                            return Ok::<(), anyhow::Error>(());
                        };
                        if app.state != shared_types::UserAppLifecycleState::Active
                            || app.lifecycle_id != operation.lifecycle_id
                            || app.active_operations.prod.as_deref()
                                != Some(operation.operation_id.as_str())
                        {
                            return Ok(());
                        }
                        // Idle policy cannot suppress durable wake recovery. The
                        // shared helper rechecks the original acknowledged write,
                        // deadline and full identity before its terminal CAS.
                        crate::userapp_recycle::recover_expired_wake_observation(
                            state.userapp_store.as_ref(),
                            state.app_service.as_ref(),
                            &app,
                            chrono::Utc::now(),
                            state.activity.wake_timeout(),
                        )
                        .await?;
                        Ok(())
                    })
                    .await??;
                    Ok(())
                });
                continue;
            }
            if operation.kind == UserAppOperationKind::Start
                && operation.state == UserAppOperationState::RecoveryRequired
                && operation.step == "traffic_wake_observing"
            {
                let store = state.userapp_store.clone();
                tasks.push(operation.operation_id.clone(), async move {
                    store.finalize_observed_wake(&operation).await?;
                    // The existing terminal receipt scanner releases only this
                    // operation's recorded lease after the terminal CAS.
                    Ok(())
                });
                continue;
            }
            if shared_types::userapp_builder_creation_needs_runtime_receipt(&operation) {
                let state = state.clone();
                tasks.push(operation.operation_id.clone(), async move {
                    // receipt 恢复不可行（Ok(false)：runtime 无恢复证据）时，
                    // 先走陈旧 Running 的证据化对账（生产 app 50/128 形态：
                    // 无回执、无 deadline、无 binding，执行者死亡后 Running
                    // 无人转换），再穿落到围栏兜底——RecoveryRequired 记录
                    // 的原语义与状态门保持不变。否则 checkpoint 为空的
                    // EnsureBuilder 围栏被本分支永久截走，兜底 settler 永远
                    // 轮不到（测试环境 20 个站立围栏的直接成因之一）。
                    if !super::creation::reconcile_runtime_receipt(&state, &operation).await? {
                        with_stall_budget(
                            "reconcile_stale_running_creation",
                            super::creation::reconcile_stale_running_creation(&state, &operation),
                        )
                        .await??;
                        with_stall_budget(
                            "reconcile_fenced_ensure",
                            super::creation::reconcile_fenced_ensure(&state, &operation),
                        )
                        .await??;
                    }
                    Ok(())
                });
                continue;
            }
            if operation.kind == UserAppOperationKind::EnsureBuilder
                && operation.state == UserAppOperationState::RecoveryRequired
                && operation.step == "builder_created_observed"
            {
                let state = state.clone();
                tasks.push(operation.operation_id.clone(), async move {
                    with_stall_budget(
                        "reconcile_created",
                        super::creation::reconcile_created(&state, &operation),
                    )
                    .await??;
                    Ok(())
                });
                continue;
            }
            if matches!(
                operation.kind,
                UserAppOperationKind::DeleteCompute
                    | UserAppOperationKind::PurgeResources
                    | UserAppOperationKind::DeleteApplication
            ) && matches!(
                operation.state,
                UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
            ) && !shared_types::userapp_operation_has_final_evidence(&operation)
            {
                let state = state.clone();
                tasks.push(operation.operation_id.clone(), async move {
                    with_stall_budget(
                        "inspect interrupted deletion",
                        state.app_service.reconcile_interrupted_deletion(&operation),
                    )
                    .await??;
                    Ok(())
                });
                continue;
            }
            if matches!(
                operation.state,
                UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
            ) && shared_types::userapp_operation_has_final_evidence(&operation)
            {
                let state = state.clone();
                tasks.push(operation.operation_id.clone(), async move {
                    if operation.kind == UserAppOperationKind::EnsureBuilder {
                        with_stall_budget(
                            "reconcile_completed",
                            super::creation::reconcile_completed(&state, &operation),
                        )
                        .await??;
                    } else if matches!(
                        operation.kind,
                        UserAppOperationKind::StopBuilder | UserAppOperationKind::RestartBuilder
                    ) {
                        with_stall_budget(
                            "control reconcile_completed",
                            super::control::reconcile_completed(&state, &operation),
                        )
                        .await??;
                    } else {
                        state.app_service.resume_pending_control(&operation).await?;
                    }
                    Ok(())
                });
                continue;
            }
            if operation.state == UserAppOperationState::RecoveryRequired
                // app_manager 侧已认领的 step 级 resume（hot_execution /
                // hot_converging / traffic_wake_observing）在前面的分支优
                // 先处理；此处兜底其余全部 kind 的证据化收束。
                && !matches!(operation.step.as_str(),
                    "hot_execution" | "hot_converging" | "traffic_wake_observing")
            {
                // Fenced operation without a step-specific resume: settle
                // automatically when the physical state is verifiably definite
                // (per-kind evidence predicates in creation::observe_fence_
                // evidence) — frees the slot without operator action.
                let state = state.clone();
                tasks.push(operation.operation_id.clone(), async move {
                    with_stall_budget(
                        "reconcile_fenced_ensure",
                        super::creation::reconcile_fenced_ensure(&state, &operation),
                    )
                    .await??;
                    Ok(())
                });
                continue;
            }
            if operation.state != UserAppOperationState::Pending
                || (operation.command.is_none()
                    && !matches!(
                        operation.kind,
                        UserAppOperationKind::EnsureBuilder
                            | UserAppOperationKind::AdoptBuilder
                            | UserAppOperationKind::AdoptApplication
                    ))
            {
                continue;
            }
            let state = state.clone();
            tasks.push(operation.operation_id.clone(), async move {
                if matches!(
                    operation.kind,
                    UserAppOperationKind::StopBuilder | UserAppOperationKind::RestartBuilder
                ) {
                    let _resumed = super::control::resume_pending(&state, &operation).await?;
                } else if operation.kind == UserAppOperationKind::AdoptBuilder {
                    let _resumed = super::adoption::resume_pending(&state, &operation).await?;
                } else if operation.kind == UserAppOperationKind::AdoptApplication {
                    let _resumed = super::app_adoption::resume_pending(&state, &operation).await?;
                } else if operation.command.is_some() {
                    state.app_service.resume_pending_control(&operation).await?;
                } else {
                    let _resumed = super::creation::resume_pending(&state, &operation).await?;
                }
                Ok(())
            });
        }
        // All valid rows were considered. Include any malformed suffix in the
        // cursor, rather than repeatedly rediscovering that historical tail.
        *cursor = raw_tail;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocked_recovery_does_not_block_other_applications_or_duplicate_itself() {
        let mut tasks = RecoveryTasks::default();
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        assert!(tasks.push("slow".into(), async move {
            wait.await?;
            Ok(())
        }));
        assert!(!tasks.push("slow".into(), async {
            panic!("duplicate must never execute")
        }));
        assert!(tasks.push("fast".into(), async { Ok(()) }));
        let (id, result) = tokio::time::timeout(Duration::from_secs(1), tasks.next())
            .await
            .expect("completion deadline")
            .expect("fast result");
        assert_eq!(id, "fast");
        result.expect("fast success");
        assert!(tasks.active.contains("slow"));
        release.send(()).expect("release slow recovery");
        let (id, result) = tokio::time::timeout(Duration::from_secs(1), tasks.next())
            .await
            .expect("completion deadline")
            .expect("slow result");
        assert_eq!(id, "slow");
        result.expect("slow success");
        assert!(tasks.active.is_empty());
    }

    #[tokio::test]
    async fn spawned_worker_keeps_its_slot_until_completion() {
        let mut tasks = RecoveryTasks::default();
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let worker = tokio::spawn(async move {
            wait.await?;
            Ok::<(), anyhow::Error>(())
        });
        assert!(tasks.push("builder".into(), async move {
            worker.await??;
            Ok(())
        }));
        for index in 1..MAX_RECOVERIES {
            assert!(tasks.push(format!("held-{index}"), std::future::pending()));
        }
        // Poll the scheduler without releasing the worker. Merely handing work
        // to tokio must not complete recovery or admit another operation.
        assert!(tasks.next().now_or_never().is_none());
        assert!(tasks.is_full());
        assert!(!tasks.push("overflow".into(), async { Ok(()) }));

        release.send(()).expect("release builder worker");
        let (id, result) = tokio::time::timeout(Duration::from_secs(1), tasks.next())
            .await
            .expect("worker completion deadline")
            .expect("worker completion");
        assert_eq!(id, "builder");
        result.expect("worker success");
        assert!(!tasks.is_full());
    }

    #[tokio::test]
    async fn capacity_and_panic_do_not_drop_other_recoveries() {
        let mut tasks = RecoveryTasks::default();
        for index in 0..MAX_RECOVERIES - 1 {
            assert!(tasks.push(format!("held-{index}"), std::future::pending()));
        }
        assert!(tasks.push("panic".into(), async {
            panic!("infrastructure fault injection")
        }));
        assert!(tasks.is_full());
        assert!(!tasks.push("overflow".into(), async { Ok(()) }));
        let (id, result) = tokio::time::timeout(Duration::from_secs(1), tasks.next())
            .await
            .expect("panic isolation deadline")
            .expect("panic result");
        assert_eq!(id, "panic");
        assert!(result.is_err());
        assert_eq!(tasks.active.len(), MAX_RECOVERIES - 1);
        assert!(tasks.push("replacement".into(), async { Ok(()) }));
        let (id, result) = tasks.next().await.expect("replacement result");
        assert_eq!(id, "replacement");
        result.expect("replacement success");
    }
}

#[cfg(all(test, unix))]
mod terminal_tests;

/// discover 全链复现：EnsureBuilder 围栏经扫描器分发→settler 收束。
/// 线上 20 个围栏零收束（零日志）而静态路径全通——此测试直接跑分发层，
/// 暴露分发顺序/任务池/谓词任何一环的问题。
#[cfg(test)]
mod discover_settle_tests {
    use super::*;
    use crate::userapp_builder::creation::fence_settler_tests::stale_running_state;
    #[cfg(feature = "rcoder-pg")]
    use crate::userapp_builder::creation::fence_settler_tests::{
        FenceRuntime, fence_state, fence_state_pg, settled,
    };
    #[cfg(not(feature = "rcoder-pg"))]
    use crate::userapp_builder::creation::fence_settler_tests::{
        FenceRuntime, fence_state, settled,
    };

    /// PG 后端复现（集群形态）：线上 20 个围栏零收束而 Turso 全通——
    /// 此测试直接验证 Postgres 后端下 discover→settler 链路。
    #[cfg(feature = "rcoder-pg")]
    #[tokio::test]
    async fn discover_settles_fence_on_postgres_backend() {
        let Some(dsn) = std::env::var("RCODER_PG_TEST_DSN")
            .ok()
            .filter(|value| !value.is_empty())
        else {
            assert_ne!(
                std::env::var("RCODER_PG_TEST_STRICT").as_deref(),
                Ok("1"),
                "Strict PG discovery tests require an explicit disposable DSN"
            );
            eprintln!("[skip] RCODER_PG_TEST_DSN not set");
            return;
        };
        // discover intentionally scans every unfinished operation. A private
        // schema isolates this fixture's controlled runtime without filtering
        // the real scanner or deleting unrelated historical test records.
        let schema = format!("fence_discovery_{}", uuid::Uuid::new_v4().simple());
        eprintln!("Real PG discovery fixture schema: {schema}");
        let mut admin = toasty::Db::builder().connect(&dsn).await.expect("PG admin");
        toasty::sql::statement(format!("CREATE SCHEMA {schema}"))
            .exec(&mut admin)
            .await
            .expect("create owned discovery schema");
        let mut scoped = url::Url::parse(&dsn).expect("PG DSN URL");
        let parameters = scoped
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        let options = parameters
            .iter()
            .filter(|(key, _)| key == "options")
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        {
            let mut query = scoped.query_pairs_mut();
            query.clear();
            query.extend_pairs(
                parameters
                    .iter()
                    .filter(|(key, _)| key != "options")
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            );
            query.append_pair("options", &format!("{options} -csearch_path={schema}"));
        }
        // PostgreSQL URI options require percent-encoded spaces. Restrict this
        // conversion to the generated query; never alter userinfo/password '+'.
        let postgres_query = scoped.query().map(|query| query.replace('+', "%20"));
        scoped.set_query(postgres_query.as_deref());
        let scoped_dsn: String = scoped.into();
        let operation_id = format!("op-pg-{}", uuid::Uuid::new_v4().simple());
        let runtime = FenceRuntime::scenario(None, None);
        *runtime.builder_workload.lock().expect("lock") = Some("rcoder-app-builder-fenced".into());
        let (state, store) = fence_state_pg(
            runtime,
            UserAppOperationKind::EnsureBuilder,
            &operation_id,
            &scoped_dsn,
        )
        .await;
        let mut tasks = RecoveryTasks::default();
        let mut cursor = None;
        discover(&state, &mut tasks, &mut cursor)
            .await
            .expect("discover");
        while let Some((id, result)) = tasks.next().await {
            result.unwrap_or_else(|error| panic!("recovery task {id} failed: {error}"));
        }
        let record = settled(&state, &operation_id).await;
        assert_eq!(
            record.state,
            UserAppOperationState::Failed,
            "PG backend must settle the fence; state={:?} step={} err={:?}",
            record.state,
            record.step,
            record.error_message
        );
        store
            .shutdown()
            .await
            .expect("stop discovery fixture writer");
        drop(state);
        toasty::sql::statement(format!("DROP SCHEMA {schema} CASCADE"))
            .exec(&mut admin)
            .await
            .expect("drop owned discovery schema");
    }

    #[tokio::test]
    async fn discover_dispatches_ensure_builder_fence_to_settler() {
        let operation_id = "op-discover-settle";
        let runtime = FenceRuntime::scenario(None, None);
        *runtime.builder_workload.lock().expect("lock") = Some("rcoder-app-builder-fenced".into());
        let (state, _dir) =
            fence_state(runtime, UserAppOperationKind::EnsureBuilder, operation_id).await;
        let mut tasks = RecoveryTasks::default();
        let mut cursor = None;
        // discover 是本模块私有 fn：直接调用，跑完整分发。
        discover(&state, &mut tasks, &mut cursor)
            .await
            .expect("discover");
        // 收集全部任务结果（不应有失败）。
        while let Some((id, result)) = tasks.next().await {
            result.unwrap_or_else(|error| panic!("recovery task {id} failed: {error}"));
        }
        let record = settled(&state, operation_id).await;
        assert_eq!(
            record.state,
            UserAppOperationState::Failed,
            "discover must settle the fence; state={:?} step={} err={:?}",
            record.state,
            record.step,
            record.error_message
        );
    }

    /// 陈旧 Running/claimed 反例（生产 app 50/128 形态：无回执、无 deadline、
    /// 无租约绑定，执行者死亡且未写任何中断记录）。修复前：回执恢复返回
    /// false 后围栏 settler 因状态门跳过 Running，记录永久停留、dev slot
    /// 永久占用。修复后：discover 必须把该记录证据化收束为 Failed 并释放
    /// slot，随后同 scope 的新请求可正常受理。
    #[tokio::test]
    async fn discover_settles_stale_running_builder_without_receipt() {
        let operation_id = "op-stale-running";
        let runtime = FenceRuntime::scenario(None, None);
        *runtime.builder_workload.lock().expect("lock") = Some("rcoder-app-builder-fenced".into());
        *runtime.unbound_creation_authority.lock().expect("lock") = Some(
            container_runtime_api::UnboundBuilderCreationAuthority::ExpiredOriginal {
                holder: "executor-fenced:op-stale-running".into(),
            },
        );
        let (state, _dir) = stale_running_state(runtime, operation_id, 1).await;
        // 确认年龄门：claimed revision 的更新时间需 ≥ ensure_timeout（1s）。
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let mut tasks = RecoveryTasks::default();
        let mut cursor = None;
        discover(&state, &mut tasks, &mut cursor)
            .await
            .expect("discover");
        while let Some((id, result)) = tasks.next().await {
            result.unwrap_or_else(|error| panic!("recovery task {id} failed: {error}"));
        }
        let record = settled(&state, operation_id).await;
        assert_eq!(
            record.state,
            UserAppOperationState::Failed,
            "stale Running must settle via evidence; state={:?} step={} err={:?}",
            record.state,
            record.step,
            record.error_message
        );
        assert!(
            record
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("stale running")),
            "release note must classify the settlement: {:?}",
            record.error_message
        );
        // 端到端：dev slot 已释放——同 app 同 scope 新受理必须 Accepted。
        let admitted = state
            .userapp_store
            .admit_with_input(
                &shared_types::UserAppAdmission {
                    app_id: "fenced".into(),
                    lifecycle_id: Some(record.lifecycle_id.clone()),
                    operation_id: format!("{operation_id}-retry"),
                    request_id: Some(format!("req-{operation_id}-retry")),
                    request_fingerprint: "cd".repeat(32),
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
            matches!(admitted, shared_types::UserAppAdmissionOutcome::Accepted(_)),
            "slot must be freed after settlement: {admitted:?}"
        );
    }
}
