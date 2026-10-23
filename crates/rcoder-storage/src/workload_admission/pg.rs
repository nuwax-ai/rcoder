//! Dedicated bounded sessions. A session is never recycled into a business pool
//! while an advisory lock or a response may be outstanding.
use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc, oneshot};

mod registration;
#[cfg(test)]
mod tests;

pub(crate) struct AdmissionPool {
    pub(crate) owner: Arc<()>,
    config: crate::config::PostgresConfig,
    connector: toasty_driver_postgresql::PostgreSQL,
    slots: Arc<Semaphore>,
    closing: Mutex<bool>,
    cancel: tokio_util::sync::CancellationToken,
    active: AtomicUsize,
    changed: tokio::sync::Notify,
}
impl AdmissionPool {
    pub(crate) fn new(config: &crate::config::PostgresConfig) -> Result<Arc<Self>> {
        let size = usize::try_from(config.max_connections())?;
        ensure!(size > 0, "Workload admission pool size must be positive");
        let dsn = config
            .to_dsn()
            .map_err(|error| connector_error("configuration", error))?;
        let connector = toasty_driver_postgresql::PostgreSQL::new(dsn)
            .map_err(|error| connector_error("configuration", error))?;
        Ok(Arc::new(Self {
            owner: Arc::new(()),
            config: config.clone(),
            connector,
            slots: Arc::new(Semaphore::new(size)),
            closing: Mutex::new(false),
            cancel: tokio_util::sync::CancellationToken::new(),
            active: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        }))
    }
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        key: WorkloadKey,
        mode: WorkloadAdmissionMode,
        deadline: Instant,
    ) -> Result<WorkloadAdmissionGuard> {
        ensure!(
            Instant::now() < deadline,
            "Workload admission deadline expired"
        );
        let slot = tokio::time::timeout_at(deadline, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| anyhow::anyhow!("Workload admission pool deadline expired"))??;
        let (commands, receiver) = mpsc::channel(4);
        let (ready, started) = oneshot::channel();
        let valid = Arc::new(AtomicBool::new(false));
        {
            let closing = self
                .closing
                .lock()
                .map_err(|_| anyhow::anyhow!("Workload admission pool poisoned"))?;
            ensure!(!*closing, "Workload admission pool is closing");
            self.active.fetch_add(1, Ordering::AcqRel);
            let pool = self.clone();
            let key = key.clone();
            let valid = valid.clone();
            tokio::spawn(async move {
                let _active = ActiveSession(pool.clone());
                let _slot = slot;
                run(pool, key, mode, deadline, receiver, ready, valid).await;
            });
        }
        tokio::time::timeout_at(deadline, started)
            .await
            .map_err(|_| anyhow::anyhow!("Workload admission deadline expired"))???;
        Ok(WorkloadAdmissionGuard {
            key,
            mode,
            owner: self.owner.clone(),
            inner: Guard::Postgres(PgGuard { commands, valid }),
        })
    }
    pub(crate) async fn shutdown(&self, deadline: Instant) -> Result<()> {
        {
            let mut closing = self
                .closing
                .lock()
                .map_err(|_| anyhow::anyhow!("Workload admission pool poisoned"))?;
            *closing = true;
            self.slots.close();
            self.cancel.cancel();
        }
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return Ok(());
            }
            tokio::time::timeout_at(deadline, notified)
                .await
                .map_err(|_| anyhow::anyhow!("Workload admission sessions remain open"))?;
        }
    }
}

fn connector_error(stage: &str, error: impl std::fmt::Display) -> anyhow::Error {
    // Do not attach the original error as a source: the official connector's
    // invalid-URL diagnostics can contain the complete credential-bearing URL.
    anyhow::anyhow!(
        "Workload admission {stage}: {}",
        shared_types::sanitize_error_text(&error.to_string())
    )
}
struct ActiveSession(Arc<AdmissionPool>);
impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.notify_waiters();
    }
}
enum Command {
    Verify(oneshot::Sender<Result<()>>),
    References(oneshot::Sender<Result<bool>>),
    Capture(oneshot::Sender<Result<WorkloadRegistrationFrame>>),
    Retire(
        Box<WorkloadRegistrationFrame>,
        oneshot::Sender<Result<PersistenceWriteOutcome>>,
    ),
    #[cfg(test)]
    BackendPid(oneshot::Sender<Result<i32>>),
    #[cfg(test)]
    BackendTls(oneshot::Sender<Result<(i32, bool)>>),
}
pub(super) struct PgGuard {
    commands: mpsc::Sender<Command>,
    valid: Arc<AtomicBool>,
}
impl PgGuard {
    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> Command,
    ) -> Result<T, AdmissionVerifyError> {
        if !self.valid.load(Ordering::Acquire) {
            return Err(AdmissionVerifyError::SessionLost);
        }
        let (send, receive) = oneshot::channel();
        self.commands
            .send(make(send))
            .await
            .map_err(|_| AdmissionVerifyError::SessionClosed)?;
        receive
            .await
            .map_err(|_| AdmissionVerifyError::OutcomeUnknown)?
            .map_err(AdmissionVerifyError::ProbeFailed)
    }
    pub(super) async fn verify(&self) -> Result<(), AdmissionVerifyError> {
        self.request(Command::Verify).await
    }
    pub(super) async fn has_references(&self) -> Result<bool> {
        self.request(Command::References)
            .await
            .map_err(anyhow::Error::new)
    }
    pub(super) async fn capture(&self) -> Result<WorkloadRegistrationFrame> {
        self.request(Command::Capture)
            .await
            .map_err(anyhow::Error::new)
    }
    pub(super) async fn retire(
        &self,
        frame: WorkloadRegistrationFrame,
    ) -> Result<PersistenceWriteOutcome> {
        Ok(
            match self
                .request(|send| Command::Retire(Box::new(frame), send))
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => PersistenceWriteOutcome::Deferred {
                    reason: format!("Registration retirement outcome is unconfirmed: {error}"),
                },
            },
        )
    }
}
async fn run(
    pool: Arc<AdmissionPool>,
    key: WorkloadKey,
    mode: WorkloadAdmissionMode,
    deadline: Instant,
    mut commands: mpsc::Receiver<Command>,
    ready: oneshot::Sender<Result<()>>,
    valid: Arc<AtomicBool>,
) {
    let connected = async {
        pool.connector
            .connect_supervised(Duration::from_secs(pool.config.connect_timeout_secs()))
            .await
            .map_err(|error| connector_error("connection/TLS handshake", error))
    };
    let connected = tokio::select! {
        _ = pool.cancel.cancelled() => Err(anyhow::anyhow!("Workload admission is closing")),
        result = tokio::time::timeout_at(deadline, connected) => result.map_err(|_| anyhow::anyhow!("Workload admission connection deadline expired")).and_then(|result| result),
    };
    let (mut client, connection) = match connected {
        Ok(value) => value,
        Err(error) => {
            let _reply = ready.send(Err(error));
            return;
        }
    };
    let live = valid.clone();
    let lost = Arc::new(tokio::sync::Notify::new());
    let lost_driver = lost.clone();
    let driver = tokio::spawn(async move {
        let result = connection.await;
        live.store(false, Ordering::Release);
        lost_driver.notify_one();
        result
    });
    let lock = match mode {
        WorkloadAdmissionMode::Shared => {
            "SELECT pg_try_advisory_lock_shared(hashtextextended($1, 719325))"
        }
        WorkloadAdmissionMode::Exclusive => {
            "SELECT pg_try_advisory_lock(hashtextextended($1, 719325))"
        }
    };
    let name = key.lock_name();
    let acquired = async {
        let statement_timeout = pool
            .config
            .statement_timeout_secs()
            .checked_mul(1000)
            .ok_or_else(|| anyhow::anyhow!("Workload admission statement timeout overflow"))?
            .to_string();
        client
            .query_one(
                "SELECT set_config('statement_timeout',$1,false)",
                &[&statement_timeout],
            )
            .await?;
        loop {
            let row = client.query_one(lock, &[&name]).await?;
            if row.try_get::<_, bool>(0)? {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let acquired = tokio::select! {
        _ = pool.cancel.cancelled() => Err(anyhow::anyhow!("Workload admission is closing")),
        result = tokio::time::timeout_at(deadline, acquired) => result.map_err(|_| anyhow::anyhow!("Workload admission lock deadline expired")).and_then(|result| result),
    };
    match acquired {
        Err(error) => {
            let _reply = ready.send(Err(error));
        }
        Ok(()) => {
            valid.store(true, Ordering::Release);
            if ready.send(Ok(())).is_ok() {
                let budget = pool.config.durable_commit_timeout();
                loop {
                    let command = tokio::select! { biased; _ = pool.cancel.cancelled() => None, _ = lost.notified() => None, command = commands.recv() => command };
                    let Some(command) = command else {
                        break;
                    };
                    if !valid.load(Ordering::Acquire) {
                        break;
                    }
                    let result = tokio::time::timeout(
                        budget,
                        execute(&mut client, &key, &pool.owner, command),
                    )
                    .await;
                    if !matches!(result, Ok(Ok(()))) {
                        break;
                    }
                }
            }
        }
    }
    valid.store(false, Ordering::Release);
    // Dropping the entire dedicated session, never returning a possibly locked
    // connection to the shared writer pool, is the release boundary.
    drop(client);
    driver.abort();
    let _closed = driver.await;
}
async fn execute(
    client: &mut tokio_postgres::Client,
    key: &WorkloadKey,
    owner: &Arc<()>,
    command: Command,
) -> Result<()> {
    match command {
        Command::Verify(reply) => {
            let result = client
                .simple_query("SELECT 1")
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from);
            let failed = result.is_err();
            let _reply = reply.send(result);
            ensure!(!failed, "Workload admission connection failed");
        }
        Command::References(reply) => {
            let result = registration::has_references(client, key).await;
            let failed = result.is_err();
            let _reply = reply.send(result);
            ensure!(!failed, "Workload reference query failed");
        }
        Command::Capture(reply) => {
            let result = registration::capture(client, key, owner.clone()).await;
            let failed = result.is_err();
            let _reply = reply.send(result);
            ensure!(!failed, "Workload registration capture failed");
        }
        Command::Retire(frame, reply) => {
            let result = registration::retire(client, key, &frame).await;
            let failed = result.is_err();
            let result = result.or_else(|error| {
                Ok::<_, anyhow::Error>(PersistenceWriteOutcome::Deferred {
                    reason: format!("Registration retirement outcome is unconfirmed: {error}"),
                })
            });
            let _reply = reply.send(result);
            ensure!(!failed, "Workload retirement outcome is unconfirmed");
        }
        #[cfg(test)]
        Command::BackendPid(reply) => {
            let result = client
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .map_err(anyhow::Error::from)
                .and_then(|row| row.try_get(0).map_err(anyhow::Error::from));
            let _reply = reply.send(result);
        }
        #[cfg(test)]
        Command::BackendTls(reply) => {
            let result = client
                .query_one(
                    "SELECT pid, ssl FROM pg_stat_ssl WHERE pid=pg_backend_pid()",
                    &[],
                )
                .await
                .map_err(anyhow::Error::from)
                .and_then(|row| Ok((row.try_get(0)?, row.try_get(1)?)));
            let _reply = reply.send(result);
        }
    }
    Ok(())
}
