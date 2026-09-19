//! UserApp control storage is independent of Agent project/session persistence.
use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAppStorageBackend {
    #[default]
    Auto,
    Turso,
    Postgres,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserAppStorageConfig {
    pub backend: UserAppStorageBackend,
    pub turso_path: PathBuf,
    /// None reuses PostgreSQL connection settings, never the Agent backend mode.
    #[serde(skip_serializing)]
    pub postgres: Option<rcoder_storage::config::PostgresConfig>,
    pub ensure_timeout_seconds: u64,
}

impl Default for UserAppStorageConfig {
    fn default() -> Self {
        Self {
            backend: UserAppStorageBackend::Auto,
            turso_path: PathBuf::from("data/rcoder/userapp.turso.db"),
            postgres: None,
            ensure_timeout_seconds: 90,
        }
    }
}

impl UserAppStorageConfig {
    pub fn resolved_backend(
        &self,
        mode: app_manager::AppAccessMode,
    ) -> anyhow::Result<UserAppStorageBackend> {
        use app_manager::AppAccessMode;
        let backend = match self.backend {
            UserAppStorageBackend::Auto if mode == AppAccessMode::Kubernetes => {
                UserAppStorageBackend::Postgres
            }
            UserAppStorageBackend::Auto => UserAppStorageBackend::Turso,
            backend => backend,
        };
        if mode == AppAccessMode::Kubernetes && backend != UserAppStorageBackend::Postgres {
            bail!("Kubernetes userApp control storage requires PostgreSQL");
        }
        Ok(backend)
    }

    pub(crate) fn apply_overrides(
        &mut self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<()> {
        if let Some(value) = lookup("RCODER_USERAPP_STORAGE_BACKEND") {
            self.backend = match value.trim() {
                "auto" => UserAppStorageBackend::Auto,
                // 旧值 fail-fast：Docker Compose 的 SQLite 已按 spec 一次性切换
                // 为 Turso，不静默映射（specs/userapp-turso-local-storage T3）。
                "sqlite" => bail!(
                    "RCODER_USERAPP_STORAGE_BACKEND=sqlite was replaced by turso; \
                     update the deployment configuration"
                ),
                "turso" => UserAppStorageBackend::Turso,
                "postgres" => UserAppStorageBackend::Postgres,
                _ => bail!("RCODER_USERAPP_STORAGE_BACKEND must be auto, turso or postgres"),
            };
        }
        if lookup("RCODER_USERAPP_SQLITE_PATH").is_some() {
            bail!(
                "RCODER_USERAPP_SQLITE_PATH was replaced by RCODER_USERAPP_TURSO_PATH; \
                 update the deployment configuration"
            );
        }
        if let Some(value) = lookup("RCODER_USERAPP_TURSO_PATH") {
            if value.trim().is_empty() {
                bail!("RCODER_USERAPP_TURSO_PATH must not be empty");
            }
            self.turso_path = PathBuf::from(value);
        }
        if let Some(value) = lookup("RCODER_USERAPP_ENSURE_TIMEOUT_SECONDS") {
            self.ensure_timeout_seconds =
                value.parse().context("invalid userApp ensure timeout")?;
        }
        if self.ensure_timeout_seconds == 0 || self.ensure_timeout_seconds > 3600 {
            bail!("userApp ensure timeout must be between 1 and 3600 seconds");
        }
        if let Some(value) = lookup("RCODER_USERAPP_PG_URL") {
            if value.trim().is_empty() {
                bail!("RCODER_USERAPP_PG_URL must not be empty");
            }
            self.postgres.get_or_insert_with(Default::default).url = Some(value);
        }
        Ok(())
    }

    /// 装配（trait-design §6）：返回业务接口 + 关机控制句柄；`control` 只
    /// 注入 shutdown coordinator，不进入业务消费者。
    pub async fn open(
        &self,
        mode: app_manager::AppAccessMode,
        postgres_fallback: &rcoder_storage::config::PostgresConfig,
    ) -> anyhow::Result<rcoder_storage::userapp_lifecycle::OpenedUserAppStore> {
        match self.resolved_backend(mode)? {
            UserAppStorageBackend::Turso => {
                #[cfg(feature = "userapp-turso")]
                {
                    let path = if self.turso_path.is_absolute() {
                        self.turso_path.clone()
                    } else {
                        std::env::current_dir()?.join(&self.turso_path)
                    };
                    let directory = path.parent().context("Turso path must name a file")?;
                    tokio::fs::create_dir_all(directory)
                        .await
                        .context("create userApp database directory")?;
                    let store =
                        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(&path)
                            .await
                            .context("initialize userApp Turso storage")?;
                    // Turso 后端新库目录不含旧 SQLite 文件时不迁移；指向的
                    // 文件若是旧 SQLite 库，迁移在启动时 fail-fast。
                    let store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore> =
                        Arc::new(store);
                    Ok(rcoder_storage::userapp_lifecycle::OpenedUserAppStore {
                        store: store.clone(),
                        control: store,
                    })
                }
                #[cfg(not(feature = "userapp-turso"))]
                bail!("Turso userApp storage requires the userapp-turso build feature")
            }
            UserAppStorageBackend::Postgres => {
                let config = self.postgres.as_ref().unwrap_or(postgres_fallback);
                config
                    .to_dsn()
                    .map_err(anyhow::Error::msg)
                    .context("configure userApp PostgreSQL storage")?;
                #[cfg(feature = "kubernetes")]
                {
                    let store = rcoder_storage::userapp_lifecycle::PgUserAppStore::connect(config)
                        .await
                        .context("initialize userApp PostgreSQL storage")?;
                    let store: Arc<rcoder_storage::userapp_lifecycle::PgUserAppStore> =
                        Arc::new(store);
                    Ok(rcoder_storage::userapp_lifecycle::OpenedUserAppStore {
                        store: store.clone(),
                        control: store,
                    })
                }
                #[cfg(not(feature = "kubernetes"))]
                bail!("PostgreSQL userApp storage requires the kubernetes build feature")
            }
            UserAppStorageBackend::Auto => bail!("userApp storage backend was not resolved"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "userapp-turso")]
    #[tokio::test]
    async fn configured_file_is_durable_and_invalid_directory_fails() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control/userapp.turso.db");
        let config = UserAppStorageConfig {
            turso_path: path.clone(),
            ..Default::default()
        };
        let pg = rcoder_storage::config::PostgresConfig::default();
        let opened = config
            .open(app_manager::AppAccessMode::Docker, &pg)
            .await
            .unwrap();
        let store = opened.store;
        // control 与 store 同源 Arc：两个引用都释放后目录锁才归还
        drop(opened.control);
        let before = store.ensure_identity("config-app").await.unwrap();
        assert!(path.is_file());
        assert!(
            config
                .open(app_manager::AppAccessMode::Docker, &pg)
                .await
                .is_err(),
            "second instance must be rejected while the directory lock is held"
        );
        drop(store);
        // R01 修正后的不变量：目录锁在 worker 线程内、随线程退出释放——
        // Drop 只发信号不 join，锁释放是"线程退出后"而非"结构体 Drop 后"
        // 同步完成。立即重开需容忍短暂的异步收束窗口（有界轮询）。
        let restored = loop {
            match config.open(app_manager::AppAccessMode::Docker, &pg).await {
                Ok(opened) => break opened,
                Err(error) if format!("{error:#}").contains("lock acquisition failed") => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
                Err(error) => panic!("reopen after drop must succeed: {error:#}"),
            }
        };
        assert_eq!(
            restored.store.get_application("config-app").await.unwrap(),
            Some(before)
        );
        drop(restored.store);
        drop(restored.control);
        let invalid = UserAppStorageConfig {
            turso_path: path.join("cannot-be-created.turso.db"),
            ..config
        };
        assert!(
            invalid
                .open(app_manager::AppAccessMode::Docker, &pg)
                .await
                .is_err()
        );
    }
    #[test]
    fn runtime_selects_durable_backend_and_k8s_rejects_turso() {
        let mut config = UserAppStorageConfig::default();
        assert_eq!(
            config
                .resolved_backend(app_manager::AppAccessMode::Docker)
                .unwrap(),
            UserAppStorageBackend::Turso
        );
        assert_eq!(
            config
                .resolved_backend(app_manager::AppAccessMode::Kubernetes)
                .unwrap(),
            UserAppStorageBackend::Postgres
        );
        config.backend = UserAppStorageBackend::Turso;
        assert!(
            config
                .resolved_backend(app_manager::AppAccessMode::Kubernetes)
                .is_err()
        );
    }
    #[test]
    fn explicit_invalid_settings_never_fall_back_to_memory() {
        for (key, value) in [
            ("RCODER_USERAPP_STORAGE_BACKEND", "memory"),
            ("RCODER_USERAPP_STORAGE_BACKEND", "sqlite"),
            ("RCODER_USERAPP_TURSO_PATH", ""),
            ("RCODER_USERAPP_SQLITE_PATH", "/old/path"),
            ("RCODER_USERAPP_ENSURE_TIMEOUT_SECONDS", "0"),
            ("RCODER_USERAPP_ENSURE_TIMEOUT_SECONDS", "oops"),
            ("RCODER_USERAPP_PG_URL", ""),
        ] {
            let mut config = UserAppStorageConfig::default();
            assert!(
                config
                    .apply_overrides(|name| (name == key).then(|| value.into()))
                    .is_err(),
                "{key}={value} must be rejected"
            );
        }
    }
}
