//! Private database execution owner. Neither Db nor a connection escapes this runtime.
//!
//! A queued job is a complete business transaction, not an individual SQL statement.
//! Cancelling its caller drops only the reply receiver. User shutdown rejects
//! admission, drains every accepted job, drops the database and its runtime, then
//! joins the owning thread before publishing a shared shutdown result. (When a
//! panic has already frozen intake, the still-queued backlog cannot be served;
//! it is dropped and those callers see OutcomeUnknown instead.)
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
    closing: Mutex<bool>,
    stop: watch::Sender<bool>,
    completion: watch::Receiver<Completion>,
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
                closing: Mutex::new(true),
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
        anyhow::ensure!(
            queue_capacity > 0 && max_inflight > 0,
            "database queue and concurrency must be positive"
        );
        let (queue, receiver) = mpsc::channel(queue_capacity);
        let (stop, closing) = watch::channel(false);
        let (finished, completion) = watch::channel(None);
        let (ready, initialized) = oneshot::channel();
        let shared = Arc::new(Shared {
            closing: Mutex::new(false),
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
                        supervise(db, receiver, closing, worker_shared, max_inflight).await
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
                    // shutdown remains the only permanent stop.
                    if let Ok(mut closing) = shared.closing.lock() {
                        *closing = true;
                    }
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
            let closing = self
                .inner
                .closing
                .lock()
                .map_err(|_| anyhow::anyhow!("database admission lock poisoned"))?;
            anyhow::ensure!(!*closing, "database is closing; job was not admitted");
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
        {
            let mut closing = self
                .inner
                .closing
                .lock()
                .map_err(|_| anyhow::anyhow!("database admission lock poisoned"))?;
            *closing = true;
            self.inner.stop.send_replace(true);
        }
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
            // 冻结中所有调用方已放弃（sender 全 drop）：恢复无人等待，
            // 排空退出（未恢复的失败经 pending_failure 如实上报）。
            if matches!(
                queue.try_recv(),
                Err(mpsc::error::TryRecvError::Disconnected)
            ) {
                queue_drained = true;
                continue;
            }
            match reopen_probe(&db, &mut stop).await {
                Reopen::Reopened => {
                    if let Ok(mut closing) = shared.closing.lock() {
                        *closing = false;
                    }
                    intake_paused = false;
                    pending_failure = None;
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
}

/// Probe with backoff until the pool yields a fresh connection or the user
/// stops the owner. `Db::connection` acquires from the pool — the same
/// recycle path production uses after a blip — so success means the
/// transport (and a live worker) is back.
async fn reopen_probe(db: &toasty::Db, stop: &mut watch::Receiver<bool>) -> Reopen {
    loop {
        if *stop.borrow() {
            return Reopen::UserStopped;
        }
        // 真实语句探活，而非池 checkout：toasty 0.10.0 的 worker-gone 缺陷
        // 里连接对象完好、只有内部 worker 已退出——checkout 恒"成功"，
        // 假解冻后首个真实请求再次 panic，冻结-解冻循环瘫痪。BEGIN+COMMIT
        // 走 Connection 提交路径，僵尸连接在此暴露。探活必须 spawn 隔离：
        // 官方 0.10.0 的 unwrap panic 若直接 await 会沿栈杀死 supervise
        // （owner 永久终局），JoinHandle 只暴露 JoinError。
        let mut probe_db = db.clone();
        let probe = tokio::spawn(async move {
            let mut tx = probe_db.transaction().await?;
            tx.commit().await
        });
        match probe.await {
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
        tokio::select! {
            biased;
            _ = stop.changed() => {
                if *stop.borrow() {
                    return Reopen::UserStopped;
                }
            }
            _ = tokio::time::sleep(REOPEN_PROBE_BACKOFF) => {}
        }
    }
}
