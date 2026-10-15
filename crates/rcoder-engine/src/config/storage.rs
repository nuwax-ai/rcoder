//! `[storage]` 配置段（rcoder-pg 双轨切换，从 config.rs 拆出）
//!
//! backend = memory（默认，纯内存单节点）| postgres（PG 持久化）；
//! PG 连接字段经 `RCODER_PG_*` env 逐项覆盖（env > config.yml > 默认值）。

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::AppConfig;

/// 存储后端类型
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    /// 纯内存（DashMap）——docker compose / 单节点默认，行为与历史版本完全一致
    #[default]
    Memory,
    /// PostgreSQL 持久化（内存镜像 + write-behind；需编译 rcoder-pg feature）
    Postgres,
}

/// `[storage]` 配置段（rcoder-pg 双轨切换）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// 存储后端（memory | postgres），默认 memory
    pub backend: StorageBackend,
    /// PG 连接配置（backend=postgres 时生效）
    #[serde(default)]
    pub postgres: rcoder_storage::config::PostgresConfig,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            backend: StorageBackend::Memory,
            postgres: rcoder_storage::config::PostgresConfig::default(),
        }
    }
}

/// 从环境变量读取 String 覆盖 target（空值忽略，不覆盖）。
fn env_override_string(key: &str, target: &mut Option<String>) {
    if let Ok(val) = std::env::var(key)
        && !val.trim().is_empty()
    {
        info!(" {key}: <set>");
        *target = Some(val);
    }
}

/// 从环境变量读取 u16 覆盖 target。
fn env_override_u16(key: &str, target: &mut Option<u16>) {
    if let Ok(val) = std::env::var(key)
        && let Ok(v) = val.parse::<u16>()
    {
        info!(" {key}: {v}");
        *target = Some(v);
    } else if let Ok(val) = std::env::var(key) {
        warn!(" parse {key} failed: {val}");
    }
}

/// 应用 storage 段的环境变量覆盖 + fail-fast 校验（load_config_with_args 调用）
/// 校验失败向上传播（fail fast，load_config_with_args 直接报错退出）
pub(crate) fn apply_storage_env_overrides(config: &mut AppConfig) -> anyhow::Result<()> {
    // 应用存储后端配置的环境变量覆盖（RCODER_STORAGE_* / RCODER_PG_*，env > config.yml > 默认）
    if let Ok(val) = std::env::var("RCODER_STORAGE_BACKEND") {
        match val.trim().to_lowercase().as_str() {
            "memory" => config.storage.backend = StorageBackend::Memory,
            "postgres" | "postgresql" | "pg" => config.storage.backend = StorageBackend::Postgres,
            other => warn!(" parse RCODER_STORAGE_BACKEND failed: {other} (memory|postgres)"),
        }
    }
    let pg = &mut config.storage.postgres;
    env_override_string("RCODER_PG_HOST", &mut pg.host);
    env_override_u16("RCODER_PG_PORT", &mut pg.port);
    env_override_string("RCODER_PG_USERNAME", &mut pg.username);
    env_override_string("RCODER_PG_PASSWORD", &mut pg.password);
    env_override_string("RCODER_PG_DATABASE", &mut pg.database);
    env_override_string("RCODER_PG_URL", &mut pg.url);
    if let Ok(val) = std::env::var("RCODER_PG_MAX_CONNECTIONS")
        && let Ok(v) = val.parse::<u32>()
    {
        pg.max_connections = Some(v);
        info!(" RCODER_PG_MAX_CONNECTIONS: {v}");
    }
    if let Ok(val) = std::env::var("RCODER_PG_MIN_CONNECTIONS")
        && let Ok(v) = val.parse::<u32>()
    {
        pg.min_connections = Some(v);
        info!(" RCODER_PG_MIN_CONNECTIONS: {v}");
    }
    if let Ok(val) = std::env::var("RCODER_PG_MAX_LIFETIME_SECS")
        && let Ok(v) = val.parse::<u64>()
    {
        pg.max_lifetime_secs = Some(v);
        info!(" RCODER_PG_MAX_LIFETIME_SECS: {v}");
    }
    if let Ok(val) = std::env::var("RCODER_PG_CONNECT_TIMEOUT_SECS")
        && let Ok(v) = val.parse::<u64>()
    {
        pg.connect_timeout_secs = Some(v);
        info!(" RCODER_PG_CONNECT_TIMEOUT_SECS: {v}");
    }
    if let Ok(val) = std::env::var("RCODER_PG_STATEMENT_TIMEOUT_SECS")
        && let Ok(v) = val.parse::<u64>()
    {
        pg.statement_timeout_secs = Some(v);
        info!(" RCODER_PG_STATEMENT_TIMEOUT_SECS: {v}");
    }
    // fail fast：backend=postgres 但连接字段缺失（DSN 组装会列出缺失项）
    if config.storage.backend == StorageBackend::Postgres
        && let Err(missing) = config.storage.postgres.to_dsn()
    {
        return Err(anyhow::anyhow!("storage.backend=postgres: {missing}"));
    }
    Ok(())
}
