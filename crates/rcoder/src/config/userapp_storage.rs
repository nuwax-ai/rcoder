//! UserApp control storage is independent of Agent project/session persistence.
use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use shared_types::UserAppLifecycleStore;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAppStorageBackend {
    #[default]
    Auto,
    Sqlite,
    Postgres,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserAppStorageConfig {
    pub backend: UserAppStorageBackend,
    pub sqlite_path: PathBuf,
    /// None reuses PostgreSQL connection settings, never the Agent backend mode.
    #[serde(skip_serializing)]
    pub postgres: Option<rcoder_storage::config::PostgresConfig>,
    pub ensure_timeout_seconds: u64,
}

impl Default for UserAppStorageConfig {
    fn default() -> Self {
        Self {
            backend: UserAppStorageBackend::Auto,
            sqlite_path: PathBuf::from("data/rcoder/userapp.sqlite3"),
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
            UserAppStorageBackend::Auto => UserAppStorageBackend::Sqlite,
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
                "sqlite" => UserAppStorageBackend::Sqlite,
                "postgres" => UserAppStorageBackend::Postgres,
                _ => bail!("RCODER_USERAPP_STORAGE_BACKEND must be auto, sqlite or postgres"),
            };
        }
        if let Some(value) = lookup("RCODER_USERAPP_SQLITE_PATH") {
            if value.trim().is_empty() {
                bail!("RCODER_USERAPP_SQLITE_PATH must not be empty");
            }
            self.sqlite_path = PathBuf::from(value);
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

    pub async fn open(
        &self,
        mode: app_manager::AppAccessMode,
        postgres_fallback: &rcoder_storage::config::PostgresConfig,
    ) -> anyhow::Result<Arc<dyn UserAppLifecycleStore>> {
        match self.resolved_backend(mode)? {
            UserAppStorageBackend::Sqlite => {
                #[cfg(feature = "userapp-sqlite")]
                {
                    let path = if self.sqlite_path.is_absolute() {
                        self.sqlite_path.clone()
                    } else {
                        std::env::current_dir()?.join(&self.sqlite_path)
                    };
                    let directory = path.parent().context("SQLite path must name a file")?;
                    tokio::fs::create_dir_all(directory)
                        .await
                        .context("create userApp database directory")?;
                    let store =
                        rcoder_storage::userapp_lifecycle::SqliteUserAppStore::open_exclusive(
                            &path,
                        )
                        .await
                        .context("initialize userApp SQLite storage")?;
                    Ok(Arc::new(store))
                }
                #[cfg(not(feature = "userapp-sqlite"))]
                bail!("SQLite userApp storage requires the userapp-sqlite build feature")
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
                    Ok(Arc::new(store))
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

    #[cfg(feature = "userapp-sqlite")]
    #[tokio::test]
    async fn configured_file_is_durable_and_invalid_directory_fails() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control/userapp.sqlite3");
        let config = UserAppStorageConfig {
            sqlite_path: path.clone(),
            ..Default::default()
        };
        let pg = rcoder_storage::config::PostgresConfig::default();
        let store = config
            .open(app_manager::AppAccessMode::Docker, &pg)
            .await
            .unwrap();
        let before = store.ensure_identity("config-app").await.unwrap();
        assert!(path.is_file());
        assert!(
            config
                .open(app_manager::AppAccessMode::Docker, &pg)
                .await
                .is_err()
        );
        drop(store);
        let restored = config
            .open(app_manager::AppAccessMode::Docker, &pg)
            .await
            .unwrap();
        assert_eq!(
            restored.get_application("config-app").await.unwrap(),
            Some(before)
        );
        drop(restored);
        let invalid = UserAppStorageConfig {
            sqlite_path: path.join("cannot-be-created.sqlite3"),
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
    fn runtime_selects_durable_backend_and_k8s_rejects_sqlite() {
        let mut config = UserAppStorageConfig::default();
        assert_eq!(
            config
                .resolved_backend(app_manager::AppAccessMode::Docker)
                .unwrap(),
            UserAppStorageBackend::Sqlite
        );
        assert_eq!(
            config
                .resolved_backend(app_manager::AppAccessMode::Kubernetes)
                .unwrap(),
            UserAppStorageBackend::Postgres
        );
        config.backend = UserAppStorageBackend::Sqlite;
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
            ("RCODER_USERAPP_SQLITE_PATH", ""),
            ("RCODER_USERAPP_ENSURE_TIMEOUT_SECONDS", "0"),
            ("RCODER_USERAPP_ENSURE_TIMEOUT_SECONDS", "oops"),
            ("RCODER_USERAPP_PG_URL", ""),
        ] {
            let mut config = UserAppStorageConfig::default();
            assert!(
                config
                    .apply_overrides(|name| (name == key).then(|| value.into()))
                    .is_err()
            );
        }
    }
}
