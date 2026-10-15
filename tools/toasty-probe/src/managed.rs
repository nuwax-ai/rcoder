use async_trait::async_trait;
use std::{borrow::Cow, sync::Arc};
use toasty_core::{
    Result, Schema,
    driver::{Capability, ConnectContext, Connection, Driver, ExecResponse, Operation},
    schema::{
        db::{AppliedMigration, Migration},
        diff,
    },
};
#[derive(Clone, Debug, Default)]
pub struct Closed {
    pub connections: usize,
    pub driver_dropped: bool,
    pub transports: usize,
    pub transport_failed: bool,
}
#[derive(Debug)]
pub struct TrackedDriver {
    inner: Option<toasty::db::Connect>,
    closed: tokio::sync::watch::Sender<Closed>,
    pg: Option<tokio_postgres::Config>,
}
impl TrackedDriver {
    pub fn new(
        inner: toasty::db::Connect,
        pg: Option<tokio_postgres::Config>,
    ) -> (Self, tokio::sync::watch::Receiver<Closed>) {
        let (closed, rx) = tokio::sync::watch::channel(Closed::default());
        (
            Self {
                inner: Some(inner),
                closed,
                pg,
            },
            rx,
        )
    }
    fn inner(&self) -> &toasty::db::Connect {
        self.inner.as_ref().unwrap()
    }
}
impl Drop for TrackedDriver {
    fn drop(&mut self) {
        drop(self.inner.take());
        self.closed.send_modify(|s| s.driver_dropped = true);
    }
}
#[async_trait]
impl Driver for TrackedDriver {
    fn url(&self) -> Cow<'_, str> {
        self.inner().url()
    }
    fn capability(&self) -> &'static Capability {
        self.inner().capability()
    }
    fn max_connections(&self) -> Option<usize> {
        self.inner().max_connections()
    }
    async fn connect(&self, cx: &ConnectContext) -> Result<Box<dyn Connection>> {
        let inner: Box<dyn Connection> = if let Some(config) = &self.pg {
            // Probe-only connector: caller explicitly restricts this to disposable,
            // local sslmode=disable databases. This is NOT the production TLS adapter.
            let (client, transport) = config
                .connect(tokio_postgres::NoTls)
                .await
                .map_err(toasty_core::Error::connection_lost)?;
            self.closed.send_modify(|s| s.transports += 1);
            let closed = self.closed.clone();
            let task = tokio::spawn(transport);
            tokio::spawn(async move {
                let result = task.await;
                closed.send_modify(|s| {
                    s.transport_failed |= !matches!(result, Ok(Ok(())));
                    s.transports -= 1;
                });
            });
            Box::new(toasty_driver_postgresql::Connection::new(client))
        } else {
            self.inner().connect(cx).await?
        };
        self.closed.send_modify(|s| s.connections += 1);
        Ok(Box::new(TrackedConnection {
            inner: Some(inner),
            closed: self.closed.clone(),
        }))
    }
    fn generate_migration(&self, s: &diff::Schema<'_>) -> Migration {
        self.inner().generate_migration(s)
    }
    async fn reset_db(&self) -> Result<()> {
        self.inner().reset_db().await
    }
}
#[derive(Debug)]
struct TrackedConnection {
    inner: Option<Box<dyn Connection>>,
    closed: tokio::sync::watch::Sender<Closed>,
}
impl Drop for TrackedConnection {
    fn drop(&mut self) {
        drop(self.inner.take());
        self.closed.send_modify(|s| s.connections -= 1);
    }
}
#[async_trait]
impl Connection for TrackedConnection {
    async fn exec(&mut self, s: &Arc<Schema>, op: Operation) -> Result<ExecResponse> {
        self.inner.as_mut().unwrap().exec(s, op).await
    }
    fn is_valid(&self) -> bool {
        self.inner.as_ref().is_some_and(|c| c.is_valid())
    }
    async fn ping(&mut self) -> Result<()> {
        self.inner.as_mut().unwrap().ping().await
    }
    async fn push_schema(&mut self, s: &Schema) -> Result<()> {
        self.inner.as_mut().unwrap().push_schema(s).await
    }
    async fn applied_migrations(&mut self) -> Result<Vec<AppliedMigration>> {
        self.inner.as_mut().unwrap().applied_migrations().await
    }
    async fn apply_migration(&mut self, id: u64, n: &str, m: &Migration) -> Result<()> {
        self.inner.as_mut().unwrap().apply_migration(id, n, m).await
    }
}
pub async fn wait_closed(mut rx: tokio::sync::watch::Receiver<Closed>) -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let s = rx.borrow_and_update().clone();
            if s.driver_dropped && s.connections == 0 && s.transports == 0 {
                anyhow::ensure!(!s.transport_failed, "PG transport failed during closure");
                return Ok::<(), anyhow::Error>(());
            }
            rx.changed().await?;
        }
    })
    .await??;
    Ok(())
}
