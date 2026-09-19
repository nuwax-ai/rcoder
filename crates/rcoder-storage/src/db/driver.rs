//! Connection policy shared by Toasty backends. Keep the official connector
//! (including PostgreSQL TLS); quarantine failed transaction-control commands.
use async_trait::async_trait;
use std::{borrow::Cow, sync::Arc};
use toasty_core::{
    Result, Schema,
    driver::{
        Capability, ConnectContext, Connection, Driver, ExecResponse, Operation,
        operation::{RawSql, RawSqlRet},
    },
    schema::{
        db::{AppliedMigration, Migration},
        diff,
    },
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum ConnectionPolicy {
    Turso,
    #[cfg(feature = "pg")]
    Postgres {
        statement_timeout_ms: u32,
    },
}

pub(crate) struct PolicyDriver {
    inner: Box<dyn Driver>,
    policy: ConnectionPolicy,
}
impl std::fmt::Debug for PolicyDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never format the connection URL: it can contain a password.
        f.debug_struct("PolicyDriver")
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}
impl PolicyDriver {
    pub(crate) fn new(inner: impl Driver + 'static, policy: ConnectionPolicy) -> Self {
        Self {
            inner: Box::new(inner),
            policy,
        }
    }
}
#[async_trait]
impl Driver for PolicyDriver {
    fn url(&self) -> Cow<'_, str> {
        self.inner.url()
    }
    fn capability(&self) -> &'static Capability {
        self.inner.capability()
    }
    fn max_connections(&self) -> Option<usize> {
        self.inner.max_connections()
    }
    async fn connect(&self, cx: &ConnectContext) -> Result<Box<dyn Connection>> {
        let mut context = cx.clone();
        // The private input/configuration tables contain credentials. Even a
        // caller enabling verbose query logging must not log bound values.
        context.query_log.params = false;
        Ok(Box::new(PolicyConnection {
            inner: self.inner.connect(&context).await?,
            policy: self.policy,
            initialized: false,
            poisoned: false,
        }))
    }
    fn generate_migration(&self, schema: &diff::Schema<'_>) -> Migration {
        self.inner.generate_migration(schema)
    }
    async fn reset_db(&self) -> Result<()> {
        Err(toasty_core::Error::unsupported_feature(
            "database reset is not a production storage operation",
        ))
    }
}
#[derive(Debug)]
struct PolicyConnection {
    inner: Box<dyn Connection>,
    policy: ConnectionPolicy,
    initialized: bool,
    poisoned: bool,
}
impl PolicyConnection {
    async fn initialize(&mut self, schema: &Arc<Schema>) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        let result: Result<()> = async {
            match self.policy {
                ConnectionPolicy::Turso => {
                    self.check_pragma(schema, "PRAGMA journal_mode=wal", "wal")
                        .await?;
                    self.statement(schema, "PRAGMA synchronous=full".into())
                        .await?;
                    self.statement(schema, "PRAGMA foreign_keys=ON".into())
                        .await?;
                    self.check_pragma(schema, "PRAGMA foreign_keys", "1")
                        .await?;
                    self.check_pragma(schema, "PRAGMA synchronous", "2").await?;
                }
                #[cfg(feature = "pg")]
                ConnectionPolicy::Postgres {
                    statement_timeout_ms,
                } => {
                    self.statement(
                        schema,
                        format!("SET statement_timeout = {statement_timeout_ms}"),
                    )
                    .await?;
                    // Statement timeout alone does not cover a suspended owner
                    // between SQL statements. Both guards apply on every pooled
                    // connection, including fresh connections after a failure.
                    // These settings are supported by PostgreSQL 16 as well.
                    let lock_timeout_ms = statement_timeout_ms.clamp(1, 2_000);
                    let idle_timeout_ms = statement_timeout_ms.clamp(1, 30_000);
                    self.statement(schema, format!("SET lock_timeout = {lock_timeout_ms}"))
                        .await?;
                    self.statement(
                        schema,
                        format!("SET idle_in_transaction_session_timeout = {idle_timeout_ms}"),
                    )
                    .await?;
                }
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            self.poisoned = true;
        }
        result?;
        self.initialized = true;
        Ok(())
    }
    async fn statement(&mut self, schema: &Arc<Schema>, sql: String) -> Result<()> {
        self.inner
            .exec(
                schema,
                RawSql {
                    sql,
                    params: vec![],
                    ret: RawSqlRet::None,
                }
                .into(),
            )
            .await?;
        Ok(())
    }
    async fn check_pragma(
        &mut self,
        schema: &Arc<Schema>,
        sql: &str,
        expected: &str,
    ) -> Result<()> {
        use toasty_core::{driver::Rows, stmt::Value};
        let response = self
            .inner
            .exec(
                schema,
                RawSql {
                    sql: sql.into(),
                    params: vec![],
                    ret: RawSqlRet::Infer,
                }
                .into(),
            )
            .await?;
        let value = match response.values {
            Rows::Count(_) => {
                return Err(toasty_core::Error::driver_operation_failed(
                    std::io::Error::other("PRAGMA returned no value"),
                ));
            }
            rows => rows.collect_as_value().await?,
        };
        let actual = match &value {
            Value::List(rows) if rows.len() == 1 => match &rows[0] {
                Value::Record(record) if record.fields.len() == 1 => match &record.fields[0] {
                    Value::String(value) => Some(value.clone()),
                    Value::I64(value) => Some(value.to_string()),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        if actual.as_deref() != Some(expected) {
            return Err(toasty_core::Error::driver_operation_failed(
                std::io::Error::other(format!(
                    "durability configuration verification failed for {sql}"
                )),
            ));
        }
        Ok(())
    }
}
#[async_trait]
impl Connection for PolicyConnection {
    async fn exec(&mut self, schema: &Arc<Schema>, operation: Operation) -> Result<ExecResponse> {
        if self.poisoned {
            return Err(toasty_core::Error::connection_lost(std::io::Error::other(
                "connection quarantined after an uncertain transaction boundary",
            )));
        }
        self.initialize(schema).await?;
        let transaction_boundary = matches!(operation, Operation::Transaction(_));
        let result = self.inner.exec(schema, operation).await;
        // BEGIN/COMMIT/ROLLBACK/savepoint errors can leave the physical connection
        // in a different transaction from the caller's view. Never recycle it.
        // Toasty checks is_valid before delivering this response, closing the
        // pool slot synchronously and preventing a racing checkout.
        self.poisoned |= transaction_boundary && result.is_err();
        result
    }
    fn is_valid(&self) -> bool {
        !self.poisoned && self.inner.is_valid()
    }
    async fn ping(&mut self) -> Result<()> {
        self.inner.ping().await
    }
    async fn push_schema(&mut self, _: &Schema) -> Result<()> {
        Err(toasty_core::Error::unsupported_feature(
            "use versioned RCoder schema initialization",
        ))
    }
    async fn applied_migrations(&mut self) -> Result<Vec<AppliedMigration>> {
        Err(toasty_core::Error::unsupported_feature(
            "use RCoder migration ledger",
        ))
    }
    async fn apply_migration(&mut self, _: u64, _: &str, _: &Migration) -> Result<()> {
        Err(toasty_core::Error::unsupported_feature(
            "use versioned RCoder schema initialization",
        ))
    }
}

#[cfg(all(test, feature = "userapp-turso"))]
mod tests {
    use super::*;
    use toasty_core::driver::operation::Transaction;

    #[derive(Debug)]
    struct BoundaryFailure;
    #[async_trait]
    impl Connection for BoundaryFailure {
        async fn exec(&mut self, _: &Arc<Schema>, operation: Operation) -> Result<ExecResponse> {
            if matches!(operation, Operation::Transaction(_)) {
                Err(toasty_core::Error::driver_operation_failed(
                    std::io::Error::other("injected transaction boundary failure"),
                ))
            } else {
                Ok(ExecResponse::count(0))
            }
        }
        fn is_valid(&self) -> bool {
            true
        }
        async fn ping(&mut self) -> Result<()> {
            Ok(())
        }
        async fn push_schema(&mut self, _: &Schema) -> Result<()> {
            Ok(())
        }
        async fn applied_migrations(&mut self) -> Result<Vec<AppliedMigration>> {
            Ok(vec![])
        }
        async fn apply_migration(&mut self, _: u64, _: &str, _: &Migration) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn failed_begin_commit_rollback_and_savepoint_are_quarantined() {
        let db = toasty::Db::builder()
            .connect("turso::memory:")
            .await
            .unwrap();
        for boundary in [
            Transaction::start(),
            Transaction::Commit,
            Transaction::Rollback,
            Transaction::Savepoint("probe".into()),
            Transaction::RollbackToSavepoint("probe".into()),
        ] {
            let mut connection = PolicyConnection {
                inner: Box::new(BoundaryFailure),
                policy: ConnectionPolicy::Turso,
                initialized: true,
                poisoned: false,
            };
            assert!(connection.exec(db.schema(), boundary.into()).await.is_err());
            assert!(!connection.is_valid());
            assert!(
                connection
                    .exec(
                        db.schema(),
                        RawSql {
                            sql: "SELECT 1".into(),
                            params: vec![],
                            ret: RawSqlRet::Infer
                        }
                        .into()
                    )
                    .await
                    .is_err()
            );
        }
    }
}
