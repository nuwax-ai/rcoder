//! Shared PostgreSQL connection policy; no Agent tasks or migrations.
use crate::config::PostgresConfig;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use tracing::info;

pub(crate) async fn connect_pool(config: &PostgresConfig) -> anyhow::Result<sqlx::PgPool> {
    let dsn = config.to_dsn().map_err(anyhow::Error::msg)?;
    // 语句超时在 acquire 后的会话级设置（防单条慢查询拖死连接）
    let statement_timeout_ms = config.statement_timeout_secs() * 1000;
    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections())
        // 预热连接（启动即建，冷启动首批查询免付建连延迟）
        .min_connections(config.min_connections())
        .acquire_timeout(Duration::from_secs(config.connect_timeout_secs()))
        // 连接最大寿命：CNPG failover 后指向旧 primary 的僵尸连接，到期在
        // release/recycle 时关闭重建即自愈（默认 600s，见 config.rs 注释）
        .max_lifetime(Duration::from_secs(config.max_lifetime_secs()))
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                // SET 语句不支持参数占位符；set_config 是等价的标准做法且可参数化
                // （sqlx 新版 SqlSafeStr 约束也禁止 format! 动态拼 SQL）
                sqlx::query("SELECT set_config('statement_timeout', $1, false)")
                    .bind(statement_timeout_ms.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&dsn)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "PG connect failed ({}/{}): {e}",
                config.host.as_deref().unwrap_or("?"),
                config.database.as_deref().unwrap_or("?")
            )
        })?;
    info!(
        "[STORAGE_PG] connected: {} (password not logged)",
        config.describe()
    );
    Ok(pool)
}
