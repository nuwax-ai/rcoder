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
    apply_durable_timeout_override(pg, std::env::var("RCODER_PG_DURABLE_COMMIT_TIMEOUT_MS"))?;
    validate_storage_config(&config.storage)
}

fn apply_durable_timeout_override(
    pg: &mut rcoder_storage::config::PostgresConfig,
    value: Result<String, std::env::VarError>,
) -> anyhow::Result<()> {
    const KEY: &str = "RCODER_PG_DURABLE_COMMIT_TIMEOUT_MS";
    let value = match value {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(()),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("{KEY} must be a Unicode positive integer in milliseconds");
        }
    };
    let milliseconds = value
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("{KEY} must be a positive integer in milliseconds"))?;
    let candidate = rcoder_storage::config::PostgresConfig {
        durable_commit_timeout_ms: Some(milliseconds),
        ..pg.clone()
    };
    candidate
        .validate_durable_commit_timeout()
        .map_err(|error| anyhow::anyhow!("invalid {KEY}: {error}"))?;
    pg.durable_commit_timeout_ms = Some(milliseconds);
    info!(" {KEY}: {milliseconds}");
    Ok(())
}

fn validate_storage_config(storage: &StorageConfig) -> anyhow::Result<()> {
    match storage.backend {
        StorageBackend::Memory => Ok(()),
        StorageBackend::Postgres => {
            storage
                .postgres
                .validate_durable_commit_timeout()
                .map_err(|error| anyhow::anyhow!("storage.postgres: {error}"))?;
            storage
                .postgres
                .to_dsn()
                .map_err(|error| anyhow::anyhow!("storage.backend=postgres: {error}"))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcoder_storage::config::PostgresConfig;
    use std::{env::VarError, ffi::OsString, time::Duration};

    #[test]
    fn durable_env_rejects_invalid_values_without_exposing_the_input() {
        for value in ["", "-1", "private-invalid-budget", "18446744073709551616"] {
            let mut pg = PostgresConfig::default();
            let original = pg.clone();
            let error = apply_durable_timeout_override(&mut pg, Ok(value.into()))
                .expect_err("an explicit invalid budget must not silently select the default");
            assert!(
                error
                    .to_string()
                    .contains("RCODER_PG_DURABLE_COMMIT_TIMEOUT_MS")
            );
            if !value.is_empty() {
                assert!(!error.to_string().contains(value));
            }
            assert_eq!(pg, original);
        }
    }

    #[test]
    fn durable_env_rejects_non_unicode_without_exposing_the_input() {
        let mut pg = PostgresConfig::default();
        let original = pg.clone();
        let error = apply_durable_timeout_override(
            &mut pg,
            Err(VarError::NotUnicode(OsString::from("private-budget-input"))),
        )
        .expect_err("an unreadable explicit budget must not be treated as absent");
        assert!(
            error
                .to_string()
                .contains("RCODER_PG_DURABLE_COMMIT_TIMEOUT_MS")
        );
        assert!(!error.to_string().contains("private-budget-input"));
        assert_eq!(pg, original);
    }

    #[test]
    fn durable_env_rejects_zero_and_unrepresentable_budgets_without_mutating_yaml() {
        for value in ["0", "2147483648", "18446744073709551615"] {
            let mut pg = PostgresConfig {
                durable_commit_timeout_ms: Some(600),
                ..Default::default()
            };
            let original = pg.clone();
            assert!(apply_durable_timeout_override(&mut pg, Ok(value.into())).is_err());
            assert_eq!(pg, original);
        }
    }

    #[test]
    fn durable_yaml_is_validated_when_postgres_is_selected() {
        for milliseconds in [0, u64::MAX] {
            let mut storage = StorageConfig {
                backend: StorageBackend::Postgres,
                postgres: PostgresConfig {
                    url: Some("postgres://test.invalid/test".into()),
                    durable_commit_timeout_ms: Some(milliseconds),
                    ..Default::default()
                },
            };
            let error = validate_storage_config(&storage).unwrap_err();
            assert!(error.to_string().contains("durable_commit_timeout_ms"));
            storage.backend = StorageBackend::Memory;
            validate_storage_config(&storage).unwrap();
        }
    }

    #[test]
    fn durable_env_overrides_yaml_and_absent_env_keeps_the_effective_value() {
        let mut storage: StorageConfig = serde_yaml::from_str(
            "backend: postgres\npostgres:\n  url: postgres://test.invalid/test\n  durable_commit_timeout_ms: 600\n",
        )
        .unwrap();
        apply_durable_timeout_override(&mut storage.postgres, Err(VarError::NotPresent)).unwrap();
        assert_eq!(
            storage.postgres.durable_commit_timeout(),
            Duration::from_millis(600)
        );
        apply_durable_timeout_override(&mut storage.postgres, Ok("2200".into())).unwrap();
        assert_eq!(
            storage.postgres.durable_commit_timeout(),
            Duration::from_millis(2200)
        );
        let legacy: StorageConfig = serde_yaml::from_str(
            "backend: postgres\npostgres:\n  url: postgres://test.invalid/test\n",
        )
        .unwrap();
        assert_eq!(
            legacy.postgres.durable_commit_timeout(),
            Duration::from_secs(15)
        );
        validate_storage_config(&legacy).unwrap();
    }

    #[test]
    fn embedded_postgres_timeout_defaults_match_the_runtime_defaults() {
        let config: AppConfig = serde_yaml::from_str(include_str!("rcoder_default.yml")).unwrap();
        let pg = PostgresConfig::default();
        assert_eq!(
            config.storage.postgres.statement_timeout_secs(),
            pg.statement_timeout_secs()
        );
        assert_eq!(config.storage.postgres.statement_timeout_secs(), 15);
        assert_eq!(
            config.storage.postgres.durable_commit_timeout(),
            Duration::from_secs(15)
        );
    }
}
