//! Private database execution owner. Neither Db nor a connection escapes this runtime.
//!
//! A queued job is a complete business transaction, not an individual SQL statement.
//! Cancelling its caller drops only the reply receiver. Closing rejects admission,
//! drains every accepted job, drops the database and its runtime, then joins the
//! owning thread before publishing a shared shutdown result.
use futures::FutureExt as _;
use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot, watch};

type Job =
    Box<dyn FnOnce(toasty::Db) -> futures::future::BoxFuture<'static, anyhow::Result<()>> + Send>;
type Completion = Option<Result<(), String>>;

#[derive(Clone)]
pub(crate) struct DatabaseOwner {
    inner: Arc<Shared>,
}
struct Shared {
    queue: mpsc::Sender<Job>,
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
    #[cfg(test)]
    pub(crate) fn closed_for_test() -> Self {
        let (queue, receiver) = mpsc::channel(1);
        drop(receiver);
        let (stop, _) = watch::channel(true);
        let (_, completion) = watch::channel(Some(Ok(())));
        Self {
            inner: Arc::new(Shared {
                queue,
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
        Fut: Future<Output = anyhow::Result<toasty::Db>> + 'static,
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
                        run(db, receiver, closing, max_inflight).await
                    });
                    drop(runtime);
                    result
                })();
                drop(resource);
                result
            })?;
        // The observer does not depend on a request's or host's Tokio runtime.
        // Completion means join returned, not merely that a Drop callback ran.
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
            inner: Arc::new(Shared {
                queue,
                closing: Mutex::new(false),
                stop,
                completion,
            }),
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
                    // Publish closed admission before waking the failed caller;
                    // otherwise it could enqueue another mutation in the gap
                    // before the owner observes this task's completion.
                    if let Ok(mut closing) = shared.closing.lock() {
                        *closing = true;
                    }
                    shared.stop.send_replace(true);
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
            self.inner
                .queue
                .try_send(job)
                .map_err(|error| match error {
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

async fn run(
    db: toasty::Db,
    mut queue: mpsc::Receiver<Job>,
    mut closing: watch::Receiver<bool>,
    max_inflight: usize,
) -> anyhow::Result<()> {
    let mut tasks = tokio::task::JoinSet::new();
    let mut close_seen = false;
    let mut queue_drained = false;
    let mut task_failed = false;
    while !queue_drained || !tasks.is_empty() {
        tokio::select! {
            biased;
            _ = closing.changed(), if !close_seen => {
                close_seen = true;
                queue.close();
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                if !matches!(result, Ok(Ok(()))) {
                    task_failed = true;
                    // Drain already accepted jobs, but stop admitting new work
                    // after a task panic. Shutdown must retain the failure.
                    close_seen = true;
                    queue.close();
                }
            }
            job = queue.recv(), if !queue_drained && tasks.len() < max_inflight => {
                match job {
                    Some(job) => { tasks.spawn(job(db.clone())); }
                    None => { queue_drained = true; }
                }
            }
        }
    }
    drop(db);
    anyhow::ensure!(
        !task_failed,
        "a database execution task failed during drain"
    );
    Ok(())
}
