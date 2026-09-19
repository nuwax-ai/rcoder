//! Restart discovery. Automatic replay is limited to unclaimed, matching inputs.
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

/// 恢复扫描器退出时对在途恢复任务的有界收束预算（R02：预算耗尽记录
/// 未完成项，不无限等待也不强行清理不确定资源）。
const RECOVERY_DRAIN_BUDGET: Duration = Duration::from_secs(30);

/// 启动恢复扫描器；返回任务句柄供关机协调者等待退出（R02）。
/// `shutdown_rx` 触发后停止发现新工作，等待在途恢复任务有界收束。
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
        let mut terminal_first = false;
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
                }
            }
        };
        // R02：停止接单后有界收束在途恢复任务；预算耗尽记录未完成项
        // （任务持有 AppState Weak，不因扫描器退出而被中止）。
        let deadline = tokio::time::Instant::now() + RECOVERY_DRAIN_BUDGET;
        while !tasks.active.is_empty() && tokio::time::Instant::now() < deadline {
            if let Some((operation_id, Err(error))) = tasks.next().await {
                tracing::warn!(%error, %operation_id, "UserApp recovery task failed during drain");
            }
        }
        if !tasks.active.is_empty() {
            tracing::error!(
                remaining = tasks.active.len(),
                budget_secs = RECOVERY_DRAIN_BUDGET.as_secs(),
                reason,
                "UserApp recovery drain budget exhausted; in-flight recovery tasks continue but store will close"
            );
        } else {
            tracing::info!(reason, "UserApp recovery scanner drained and exited");
        }
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
    if tasks.is_full() {
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
        if tasks.is_full() {
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
        if tasks.is_full() {
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
            if tasks.is_full() {
                return Ok(());
            }
            *cursor = Some(operation.operation_id.clone());
            if matches!(
                operation.state,
                UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
            ) && shared_types::userapp_operation_has_final_evidence(&operation)
            {
                let state = state.clone();
                tasks.push(operation.operation_id.clone(), async move {
                    if operation.kind == UserAppOperationKind::EnsureBuilder {
                        super::creation::reconcile_completed(&state, &operation).await?;
                    } else if matches!(
                        operation.kind,
                        UserAppOperationKind::StopBuilder | UserAppOperationKind::RestartBuilder
                    ) {
                        super::control::reconcile_completed(&state, &operation).await?;
                    } else {
                        state.app_service.resume_pending_control(&operation).await?;
                    }
                    Ok(())
                });
                continue;
            }
            if operation.state != UserAppOperationState::Pending
                || (operation.command.is_none()
                    && !matches!(
                        operation.kind,
                        UserAppOperationKind::EnsureBuilder | UserAppOperationKind::AdoptBuilder
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
