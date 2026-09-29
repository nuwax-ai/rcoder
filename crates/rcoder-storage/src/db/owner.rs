//! Private database execution owner. Neither Db nor a connection escapes this runtime.
//!
//! A queued job is a complete business transaction, not an individual SQL statement.
//! Cancelling its caller drops only the reply receiver. User shutdown rejects
//! admission, drains every accepted job, drops the database and its runtime, then
//! joins the owning thread before publishing a shared shutdown result. When a
//! panic has frozen intake, already-admitted backlog jobs are **served exactly
//! once** after a successful recovery probe (only the panicked job's own caller
//! sees OutcomeUnknown — it is never replayed); if shutdown wins the recovery
//! race, the backlog is dropped and those callers see OutcomeUnknown instead.
//!
//! A panicking task (for example toasty 0.10.0's connection-channel
//! `unwrap` on a transient backend blip) reports `OutcomeUnknown` to its own
//! caller and freezes admission, but is not terminal for the owner: after the
//! in-flight set drains, the owner probes the pool for a fresh connection
//! (`Db::connection` — the pool evicts dead workers on recycle) and resumes
//! admission when the probe succeeds. Only user shutdown (or a probe that
//! never succeeds until then) ends the owner.
use futures::FutureExt as _;
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};

type Job =
    Box<dyn FnOnce(toasty::Db) -> futures::future::BoxFuture<'static, anyhow::Result<()>> + Send>;
type Completion = Option<Result<(), String>>;

/// Backoff between failed reopen probes while the database stays unreachable.
const REOPEN_PROBE_BACKOFF: Duration = Duration::from_secs(1);
/// 单次探活预算：悬挂连接（永不返回的 BEGIN/COMMIT）按次放弃退避重试，
/// 不拖死整个恢复循环（DB-1 §4.2）。
const SINGLE_PROBE_BUDGET: Duration = Duration::from_secs(5);

/// 一次恢复探活的执行体（默认实现 = 真实 BEGIN/COMMIT；测试注入可控探针
/// 以确定性屏障覆盖 probe/shutdown/last-drop 竞态）。
type ProbeFn = Arc<
    dyn Fn(toasty::Db) -> futures::future::BoxFuture<'static, anyhow::Result<()>> + Send + Sync,
>;

fn real_probe(mut db: toasty::Db) -> futures::future::BoxFuture<'static, anyhow::Result<()>> {
    Box::pin(async move {
        let tx = db.transaction().await?;
        tx.commit().await?;
        anyhow::Ok(())
    })
}

#[derive(Clone)]
pub(crate) struct DatabaseOwner {
    /// 每个调用方克隆各持一份 sender；**绝不与 worker 共享**——最后一份
    /// clone drop 后队列关闭即触发 owner 排空退出（last-drop 语义）。
    queue: mpsc::Sender<Job>,
    inner: Arc<Shared>,
}
/// Worker 与调用方共享的**无队列引用**状态：worker 若持有 sender 克隆，
/// 所有调用方 drop 后队列永不关闭，owner 永不退出（资源泄漏——契约
/// `last_owner_drop_drains_accepted_job_and_releases_resource` 锁定的语义）。
struct Shared {
    admission: Mutex<Admission>,
    stop: watch::Sender<bool>,
    completion: watch::Receiver<Completion>,
}

/// 准入状态机（DB-1）：`Open -> Recovering -> Open`；`Open/Recovering ->
/// Closing`；`Closing` 不可被恢复探活改回（shutdown 与恢复提交共享同一
/// 同步边界）。这是本进程数据库 worker 的内部同步，不是业务准入或多副本锁。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Open,
    Recovering,
    Closing,
}

impl Shared {
    /// Open→Recovering（panic 冻结）；已在 Closing 则不覆盖。
    fn mark_recovering_if_open(&self) -> bool {
        match self.admission.lock() {
            Ok(mut admission) => {
                if matches!(*admission, Admission::Open) {
                    *admission = Admission::Recovering;
                    true
                } else {
                    false
                }
            }
            Err(poisoned) => {
                *poisoned.into_inner() = Admission::Closing;
                false
            }
        }
    }

    /// 任意态→Closing（shutdown 不可撤销）。
    fn begin_closing(&self) {
        match self.admission.lock() {
            Ok(mut admission) => *admission = Admission::Closing,
            Err(poisoned) => *poisoned.into_inner() = Admission::Closing,
        }
    }

    /// Recovering→Open（探活成功提交重开）；Closing 拒绝——shutdown 优先。
    fn reopen_if_recovering(&self) -> bool {
        match self.admission.lock() {
            Ok(mut admission) => {
                if matches!(*admission, Admission::Recovering) {
                    *admission = Admission::Open;
                    true
                } else {
                    false
                }
            }
            Err(poisoned) => {
                *poisoned.into_inner() = Admission::Closing;
                false
            }
        }
    }

    /// Hold this guard through try_send so shutdown/panic cannot close admission
    /// between the check and enqueue. No asynchronous work runs under this lock.
    fn open_admission(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Admission>> {
        let admission = self.admission.lock().map_err(|_| {
            anyhow::anyhow!("database admission lock poisoned; job was not admitted")
        })?;
        anyhow::ensure!(
            matches!(*admission, Admission::Open),
            "database is closing; job was not admitted"
        );
        Ok(admission)
    }
}

#[derive(Debug)]
pub(crate) struct OutcomeUnknown;
impl std::fmt::Display for OutcomeUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("database execution outcome is unknown; query the original operation identity")
    }
}
impl std::error::Error for OutcomeUnknown {}

impl DatabaseOwner {
    /// A terminal execution boundary for queue/drain failure tests; it cannot
    /// admit work or fabricate a successfully executed transaction.
    #[cfg(all(test, feature = "pg"))]
    pub(crate) fn closed_for_test() -> Self {
        let (queue, receiver) = mpsc::channel(1);
        drop(receiver);
        let (stop, _) = watch::channel(true);
        let (_, completion) = watch::channel(Some(Ok(())));
        Self {
            queue,
            inner: Arc::new(Shared {
                admission: Mutex::new(Admission::Closing),
                stop,
                completion,
            }),
        }
    }

    pub(crate) async fn open<I, Fut, Resource>(
        queue_capacity: usize,
        max_inflight: usize,
        resource: Resource,
        initialize: I,
    ) -> anyhow::Result<Self>
    where
        I: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<toasty::Db>> + Send + 'static,
        Resource: Send + 'static,
    {
        Self::open_with_probe(
            queue_capacity,
            max_inflight,
            resource,
            initialize,
            Arc::new(real_probe),
        )
        .await
    }

    /// 测试可注入探针体（确定性时序）；生产行为与 [`Self::open`] 一致。
    pub(crate) async fn open_with_probe<I, Fut, Resource>(
        queue_capacity: usize,
        max_inflight: usize,
        resource: Resource,
        initialize: I,
        probe: ProbeFn,
    ) -> anyhow::Result<Self>
    where
        I: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<toasty::Db>> + Send + 'static,
        Resource: Send + 'static,
    {
        anyhow::ensure!(
            queue_capacity > 0 && max_inflight > 0,
            "database queue and concurrency must be positive"
        );
        let (queue, receiver) = mpsc::channel(queue_capacity);
        let (stop, closing) = watch::channel(false);
        let (finished, completion) = watch::channel(None);
        let (ready, initialized) = oneshot::channel();
        let shared = Arc::new(Shared {
            admission: Mutex::new(Admission::Open),
            stop: stop.clone(),
            completion: completion.clone(),
        });
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("rcoder-db-owner".into())
            .spawn(move || {
                // Declare ownership before runtime so all failure paths drop runtime
                // (including official-driver background transports) before the lock.
                let result = (|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    let result = runtime.block_on(async move {
                        let db = match initialize().await {
                            Ok(db) => db,
                            Err(error) => {
                                let message = format!("database initialization failed: {error:#}");
                                drop(ready.send(Err(message.clone())));
                                anyhow::bail!(message);
                            }
                        };
                        if ready.send(Ok(())).is_err() {
                            // Opening caller disappeared. Do not leave an orphan owner.
                            drop(db);
                            return Ok(());
                        }
                        supervise(db, receiver, closing, worker_shared, max_inflight, probe).await
                    });
                    drop(runtime);
                    result
                })();
                drop(resource);
                result
            })?;
        // The observer does not depend on a request's runtime.
        // Completion means join returned, not merely a Drop callback ran.
        std::thread::Builder::new()
            .name("rcoder-db-join".into())
            .spawn(move || {
                let outcome = match worker.join() {
                    Ok(result) => {
                        result.map_err(|error| format!("database owner failed: {error:#}"))
                    }
                    Err(_) => {
                        Err("database owner panicked; execution outcome may be unknown".into())
                    }
                };
                finished.send_replace(Some(outcome));
            })?;
        let initialization = initialized
            .await
            .map_err(|_| anyhow::anyhow!("database owner exited during initialization"))
            .and_then(|result| result.map_err(anyhow::Error::msg));
        if let Err(error) = initialization {
            stop.send_replace(true);
            drop(queue);
            // A failed open is not complete until its runtime and directory lock
            // have been released. Keep the original initialization error; a
            // worker failure is already reflected in that error's context.
            drop(wait_for_completion(completion).await);
            return Err(error);
        }
        Ok(Self {
            queue,
            inner: shared,
        })
    }

    pub(crate) async fn execute<T, F, Fut>(&self, transaction: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(toasty::Db) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        let (reply, result) = oneshot::channel();
        let admission = Arc::downgrade(&self.inner);
        let job: Job = Box::new(move |db| {
            Box::pin(async move {
                let outcome = std::panic::AssertUnwindSafe(async move { transaction(db).await })
                    .catch_unwind()
                    .await;
                let panicked = outcome.is_err();
                if panicked && let Some(shared) = admission.upgrade() {
                    // Freeze admission before waking the failed caller so it
                    // cannot enqueue another mutation in the gap before the
                    // owner observes this task's completion. Freezing is not
                    // terminal: the owner drains, probes the pool, and
                    // reopens if a fresh connection is available. User
                    // shutdown remains the only permanent stop (Closing is
                    // never reopened by a probe).
                    shared.mark_recovering_if_open();
                }
                drop(reply.send(outcome.unwrap_or_else(|_| Err(OutcomeUnknown.into()))));
                anyhow::ensure!(
                    !panicked,
                    "database transaction task panicked; outcome requires verification"
                );
                Ok(())
            })
        });
        {
            let _admission = self.inner.open_admission()?;
            self.queue.try_send(job).map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    anyhow::anyhow!("database queue is full; job was not admitted")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    anyhow::anyhow!("database worker is unavailable; job was not admitted")
                }
            })?;
        }
        result
            .await
            .map_err(|_| anyhow::Error::new(OutcomeUnknown))?
    }

    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        // Closing 不可撤销：与恢复提交共享同一准入锁，探活成功不能覆盖。
        self.inner.begin_closing();
        self.inner.stop.send_replace(true);
        wait_for_completion(self.inner.completion.clone()).await
    }
}

async fn wait_for_completion(mut completion: watch::Receiver<Completion>) -> anyhow::Result<()> {
    loop {
        if let Some(result) = completion.borrow_and_update().clone() {
            return result.map_err(anyhow::Error::msg);
        }
        completion
            .changed()
            .await
            .map_err(|_| anyhow::anyhow!("database join observer exited without a result"))?;
    }
}

/// Serve until user shutdown. A task failure (by construction, a panic: jobs
/// return `Err` only from the panic guard) freezes intake, drains the
/// in-flight set, then probes the pool for a healthy connection. A
/// successful probe reopens admission on the same channel; the caller of the
/// panicked job has already received `OutcomeUnknown` and must verify it
/// independently — reopening never fabricates that outcome.
async fn supervise(
    db: toasty::Db,
    mut queue: mpsc::Receiver<Job>,
    mut stop: watch::Receiver<bool>,
    shared: Arc<Shared>,
    max_inflight: usize,
    probe: ProbeFn,
) -> anyhow::Result<()> {
    let mut tasks = tokio::task::JoinSet::new();
    let mut stop_seen = false;
    let mut queue_drained = false;
    let mut intake_paused = false;
    // Set when a task failed and the failure was later recovered from; only an
    // unrecovered failure at exit makes the owner report an error.
    let mut pending_failure: Option<String> = None;
    loop {
        // last-drop 语义：队列关闭（所有调用方 clone drop 或显式 stop 关闭）
        // 且在途排空即退出——不要求 stop 信号（契约
        // last_owner_drop_drains_accepted_job_and_releases_resource）。
        if queue_drained && tasks.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            _ = stop.changed(), if !stop_seen => {
                stop_seen = true;
                queue.close();
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = result.unwrap_or_else(|error| Err(anyhow::anyhow!(
                    "database task join failed: {error}"
                ))) {
                    // Freeze intake (do NOT close the channel: recovery may
                    // resume serving the already-admitted backlog).
                    intake_paused = true;
                    pending_failure = Some(format!("{error:#}"));
                }
            }
            job = queue.recv(), if !queue_drained && !intake_paused && tasks.len() < max_inflight => {
                match job {
                    Some(job) => { tasks.spawn(job(db.clone())); }
                    None => { queue_drained = true; }
                }
            }
        }
        // Drain complete after a failure: probe the pool for recovery.
        if intake_paused && tasks.is_empty() {
            // 不消费元素的排空检查（DB-1：try_recv 会把已受理任务直接丢弃
            // ——每次冻结周期牺牲一个 job，其调用方无端 OutcomeUnknown）。
            // is_closed = 全部 sender 已 drop（last-drop 语义）；is_empty
            // 确认无已受理积压——两者缺一不可：关闭后可能仍有待执行 backlog。
            if queue.is_closed() && queue.is_empty() {
                queue_drained = true;
                continue;
            }
            match reopen_probe(&db, &mut queue, &mut stop, &probe).await {
                Reopen::Reopened => {
                    // 状态机提交重开：Closing（shutdown 已受理）拒绝——停机
                    // 优先，恢复探活不得覆盖关闭意图。
                    if shared.reopen_if_recovering() {
                        intake_paused = false;
                        pending_failure = None;
                    } else {
                        // 已 Closing：与 UserStopped 同收束——积压丢弃
                        //（调用方收 OutcomeUnknown），排空退出。
                        queue.close();
                        while queue.try_recv().is_ok() {}
                        queue_drained = true;
                    }
                }
                Reopen::UserStopped => {
                    // User stop wins over recovery: the frozen owner must not
                    // execute the queued backlog either. Drop it — those
                    // callers already receive OutcomeUnknown from the dropped
                    // reply channels — and let the loop finish draining stop.
                    queue.close();
                    while queue.try_recv().is_ok() {}
                    queue_drained = true;
                }
                // DB-R4：探活期间所有调用方已消失且无已受理积压——有界退出
                // 释放资源（线程/DB/目录独占），不因探活持续失败无限保留。
                Reopen::LastDropped => {
                    queue_drained = true;
                }
            }
        }
    }
    drop(db);
    if let Some(failure) = pending_failure {
        anyhow::bail!("database execution failed and did not recover: {failure}");
    }
    Ok(())
}

enum Reopen {
    Reopened,
    UserStopped,
    /// DB-R4：探活/退避循环观察到 last-drop（队列关闭且空）——owner 应有界退出。
    LastDropped,
}

/// Probe with backoff until the pool yields a fresh connection, the user stops
/// the owner, or every caller disappears (DB-R4: last-drop during a failing
/// recovery must still release the owner's resources within a bounded window).
async fn reopen_probe(
    db: &toasty::Db,
    queue: &mut mpsc::Receiver<Job>,
    stop: &mut watch::Receiver<bool>,
    probe: &ProbeFn,
) -> Reopen {
    loop {
        if *stop.borrow() {
            return Reopen::UserStopped;
        }
        // 不消费元素的排空检查（closed = 全部 sender 已 drop；empty = 无已
        // 受理积压）。closed+empty 一旦成立即稳定（无 sender 可再入队），
        // 是安全的退出决策；已受理 backlog 存在时继续探活服务。
        if queue.is_closed() && queue.is_empty() {
            return Reopen::LastDropped;
        }
        // 真实语句探活，而非池 checkout：toasty 0.10.0 的 worker-gone 缺陷
        // 里连接对象完好、只有内部 worker 已退出——checkout 恒"成功"，
        // 假解冻后首个真实请求再次 panic，冻结-解冻循环瘫痪。BEGIN+COMMIT
        // 走 Connection 提交路径，僵尸连接在此暴露。探活必须 spawn 隔离：
        // 官方 0.10.0 的 unwrap panic 若直接 await 会沿栈杀死 supervise
        // （owner 永久终局），JoinHandle 只暴露 JoinError。
        //
        // DB-1：单次探活有界（悬挂连接不拖死等待方）；等待探活时响应
        // shutdown；取消探针后 await 收束其 task（不丢句柄让探针后台裸跑）。
        // 只取消恢复探针，不中止已执行的业务事务——job 的取消语义不由此推导。
        let mut probe_task = tokio::spawn(probe(db.clone()));
        let outcome = tokio::select! {
            biased;
            _ = stop.changed() => {
                probe_task.abort();
                // Completion may have won the race with abort. Joining either
                // result is valid; shutdown still wins this select branch.
                if let Err(error) = probe_task.await && !error.is_cancelled() {
                    tracing::warn!(%error, "database reopen probe failed while stopping");
                }
                if *stop.borrow() {
                    return Reopen::UserStopped;
                }
                continue;
            }
            _ = tokio::time::sleep(SINGLE_PROBE_BUDGET) => {
                probe_task.abort();
                if let Err(error) = probe_task.await && !error.is_cancelled() {
                    tracing::warn!(%error, "database reopen probe failed after its budget expired");
                }
                tracing::warn!(
                    "database reopen probe exceeded its budget; retrying with backoff"
                );
                tokio::select! {
                    biased;
                    _ = stop.changed() => {
                        if *stop.borrow() {
                            return Reopen::UserStopped;
                        }
                    }
                    _ = tokio::time::sleep(REOPEN_PROBE_BACKOFF) => {}
                }
                continue;
            }
            probed = &mut probe_task => probed,
        };
        match outcome {
            Ok(Ok(())) => return Reopen::Reopened,
            Ok(Err(error)) => {
                tracing::warn!(%error, "database reopen probe statement failed; retrying");
            }
            Err(join_error) => {
                tracing::warn!(
                    %join_error,
                    "database reopen probe panicked (zombie connection signature); retrying"
                );
            }
        }
        // 探活失败的退避窗口同样观察 last-drop（DB-R4）：退避期间调用方全部
        // 消失且无积压 → 有界退出，不空转下一轮探活。
        tokio::select! {
            biased;
            _ = stop.changed() => {
                if *stop.borrow() {
                    return Reopen::UserStopped;
                }
            }
            _ = tokio::time::sleep(REOPEN_PROBE_BACKOFF) => {}
        }
        if queue.is_closed() && queue.is_empty() {
            return Reopen::LastDropped;
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    fn shared() -> Shared {
        let (stop, _) = watch::channel(false);
        let (_, completion) = watch::channel(None);
        Shared {
            admission: Mutex::new(Admission::Open),
            stop,
            completion,
        }
    }

    #[tokio::test]
    async fn poisoned_admission_does_not_enqueue_a_job() {
        let shared = Arc::new(shared());
        let poisoned = shared.clone();
        assert!(
            std::thread::spawn(move || {
                let _guard = poisoned.admission.lock().unwrap();
                panic!("injected admission lock poison");
            })
            .join()
            .is_err()
        );
        let (queue, mut receiver) = mpsc::channel(1);
        let owner = DatabaseOwner {
            queue,
            inner: shared,
        };
        let result = owner.execute(|_| async { Ok(()) });
        tokio::pin!(result);
        assert!(matches!(
            futures::poll!(&mut result),
            std::task::Poll::Ready(Err(_))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    /// DB-1 §4.3-2：恢复探活成功提交重开时若 shutdown 已受理（Closing），
    /// 重开必须被拒——关闭意图不可被覆盖。
    #[test]
    fn closing_wins_over_recovery_reopen() {
        let state = shared();
        assert!(state.mark_recovering_if_open(), "Open -> Recovering");
        state.begin_closing();
        assert!(!state.reopen_if_recovering(), "Closing must not reopen");
        assert!(state.open_admission().is_err());
    }

    /// panic 冻结不得覆盖已受理的 Closing。
    #[test]
    fn panic_during_closing_keeps_closing() {
        let state = shared();
        state.begin_closing();
        assert!(!state.mark_recovering_if_open());
        assert!(state.open_admission().is_err());
    }

    /// 正常恢复周期：Open → Recovering → Open。
    #[test]
    fn normal_reopen_cycle() {
        let state = shared();
        assert!(state.mark_recovering_if_open());
        assert!(state.open_admission().is_err(), "frozen admission rejects");
        assert!(state.reopen_if_recovering());
        assert!(state.open_admission().is_ok());
    }
}
