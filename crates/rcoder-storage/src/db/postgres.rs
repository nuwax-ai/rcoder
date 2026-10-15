//! Official Toasty PG connector, with the same configured URL and pool budgets.
use crate::config::PostgresConfig;
use crate::db::{
    driver::{ConnectionPolicy, PolicyDriver},
    models,
    owner::DatabaseOwner,
    schema::{self, Backend, Component},
};
use anyhow::{Context, Result, ensure};
use std::time::Duration;

pub(crate) async fn open(
    config: &PostgresConfig,
    components: Vec<Component>,
) -> Result<DatabaseOwner> {
    let config = config.clone();
    let max_connections = usize::try_from(config.max_connections())?;
    ensure!(max_connections > 0, "PostgreSQL pool size must be positive");
    let statement_timeout_ms = u32::try_from(
        config
            .statement_timeout_secs()
            .checked_mul(1000)
            .context("PostgreSQL statement timeout overflow")?,
    )?;
    let dsn = config.to_dsn().map_err(anyhow::Error::msg)?;
    DatabaseOwner::open(256, max_connections, (), move || async move {
        let driver = PolicyDriver::new(
            toasty::db::Connect::new(&dsn).await?,
            ConnectionPolicy::Postgres {
                statement_timeout_ms,
            },
        );
        let mut db = toasty::Db::builder()
            .models(models::storage_models())
            .max_pool_size(max_connections)
            .pool_wait_timeout(Some(Duration::from_secs(config.connect_timeout_secs())))
            .pool_create_timeout(Some(Duration::from_secs(config.connect_timeout_secs())))
            .pool_pre_ping(true)
            .pool_max_connection_lifetime(Some(Duration::from_secs(config.max_lifetime_secs())))
            .build(driver)
            .await?;
        schema::initialize(&mut db, Backend::Postgres, &components).await?;
        // Hold each checkout until the configured warm set is established,
        // otherwise repeatedly acquiring one idle connection would not prewarm.
        let mut warm = Vec::new();
        for _ in 0..config.min_connections() {
            warm.push(db.connection().await?);
        }
        drop(warm);
        Ok(db)
    })
    .await
}
