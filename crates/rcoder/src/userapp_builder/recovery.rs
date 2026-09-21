//! Restart discovery. Automatic replay is limited to unclaimed, matching inputs.
mod compute;
mod discovery;
use crate::app_state::AppState;
use futures::{FutureExt as _, StreamExt as _, future::BoxFuture, stream::FuturesUnordered};
use shared_types::{UserAppOperationKind, UserAppOperationState};
use std::{
    collections::HashSet, future::Future, panic::AssertUnwindSafe, sync::Weak, time::Duration,
};

const MAX_RECOVERIES: usize = 8;
const SCAN_PAGE_SIZE: u32 = 128;
const MAX_PAGES_PER_TICK: usize = 4;
const SCAN_READ_TIMEOUT: Duration = Duration::from_secs(5);

type RecoveryResult = (String, anyhow::Result<()>);

/// Bounded process-local scheduling only. Durable claims and resource leases
/// remain authoritative; occupying a slot does not authorize a resource write.
#[derive(Default)]
struct RecoveryTasks {
    active: HashSet<String>,
    pending: FuturesUnordered<BoxFuture<'static, RecoveryResult>>,
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
                        match state
                            .runtime
                            .sweep_legacy_operation_locks(Duration::from_secs(24 * 3600))
                            .await
                        {
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
        store.terminal_operation_leases(query_cursor.as_deref(), SCAN_PAGE_SIZE),
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
    if page.is_empty() {
        *cursor = None;
        return Ok(());
    }
    for binding in page {
        if tasks.reject_if_full("terminal_lease_push") {
            break;
        }
        *cursor = Some(binding.context.operation_id.clone());
        let store = store.clone();
        let runtime = runtime.clone();
        tasks.push(binding.context.operation_id.clone(), async move {
            runtime
                .release_app_operation_receipt(&binding.context, &binding.receipt)
                .await?;
            store.forget_operation_lease(&binding).await?;
            Ok(())
        });
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
                .unfinished_operations(query_cursor.as_deref(), SCAN_PAGE_SIZE),
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
        if page.is_empty() {
            *cursor = None;
            return Ok(());
        }
        for operation in page {
            if tasks.reject_if_full("dispatch") {
                return Ok(());
            }
            *cursor = Some(operation.operation_id.clone());
            if operation.state == UserAppOperationState::RecoveryRequired
                && operation.step == "hot_execution"
                && operation
                    .checkpoint
                    .pointer("/hot_execution/receipt_protocol")
                    .and_then(serde_json::Value::as_u64)
                    == Some(1)
            {
                let state = state.clone();
                tasks.push(operation.operation_id.clone(), async move {
                    state.app_service.resume_pending_control(&operation).await?;
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
                    // receipt 恢复不可行（Ok(false)：runtime 无恢复证据）时
                    // 穿落到证据化兜底，而不是每扫描周期空转重试——否则
                    // checkpoint 为空的 EnsureBuilder 围栏被本分支永久截走，
                    // 兜底 settler 永远轮不到（测试环境 20 个站立围栏的
                    // 直接成因之一）。
                    if !super::creation::reconcile_runtime_receipt(&state, &operation).await? {
                        tokio::time::timeout(
                            Duration::from_secs(60),
                            super::creation::reconcile_fenced_ensure(&state, &operation),
                        )
                        .await
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "reconcile_fenced_ensure timed out (stalled runtime call)"
                            )
                        })??;
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
                    tokio::time::timeout(
                        Duration::from_secs(60),
                        super::creation::reconcile_created(&state, &operation),
                    )
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("reconcile_created timed out (stalled runtime call)")
                    })??;
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
                        tokio::time::timeout(
                            Duration::from_secs(60),
                            super::creation::reconcile_completed(&state, &operation),
                        )
                        .await
                        .map_err(|_| {
                            anyhow::anyhow!("reconcile_completed timed out (stalled runtime call)")
                        })??;
                    } else if matches!(
                        operation.kind,
                        UserAppOperationKind::StopBuilder | UserAppOperationKind::RestartBuilder
                    ) {
                        tokio::time::timeout(
                            Duration::from_secs(60),
                            super::control::reconcile_completed(&state, &operation),
                        )
                        .await
                        .map_err(|_| {
                            anyhow::anyhow!(
                                "control reconcile_completed timed out (stalled runtime call)"
                            )
                        })??;
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
                    tokio::time::timeout(
                        Duration::from_secs(60),
                        super::creation::reconcile_fenced_ensure(&state, &operation),
                    )
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("reconcile_fenced_ensure timed out (stalled runtime call)")
                    })??;
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
    use crate::userapp_builder::creation::fence_settler_tests::{
        FenceRuntime, fence_state, fence_state_pg, settled,
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
            eprintln!("[skip] RCODER_PG_TEST_DSN not set");
            return;
        };
        let operation_id = format!("op-pg-{}", uuid::Uuid::new_v4().simple());
        let runtime = FenceRuntime::scenario(None, None);
        *runtime.builder_workload.lock().expect("lock") = Some("rcoder-app-builder-fenced".into());
        let state = fence_state_pg(
            runtime,
            UserAppOperationKind::EnsureBuilder,
            &operation_id,
            &dsn,
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
}
