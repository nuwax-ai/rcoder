//! Durable read-only descriptions. No PID, lease, token, environment or commands.
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use workspace_manifest::{LogSource, ReleaseLock};

use crate::sources::{LogLayout, inject_orchestrator_log_source, inject_runtime_log_sources};

const MAX_CATALOG_BYTES: u64 = 1024 * 1024;

pub fn catalog_path(state_root: &Path) -> PathBuf {
    state_root.join("log-catalog.json")
}

/// Captured location for publishing by an already-owned manager.
#[derive(Clone, Debug)]
pub struct CatalogLocation {
    pub app_id: String,
    pub source_root: PathBuf,
    pub log_root: PathBuf,
    pub state_root: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceLogs {
    pub service_id: String,
    pub sources: Vec<LogSource>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogCatalog {
    pub schema_version: u32,
    pub app_id: String,
    pub source_root: PathBuf,
    pub log_root: PathBuf,
    pub layout: LogLayout,
    pub version: String,
    pub release_id: String,
    pub builtin_orchestrator: bool,
    pub services: Vec<ServiceLogs>,
}

impl LogCatalog {
    pub fn from_release(
        app_id: String,
        source_root: PathBuf,
        log_root: PathBuf,
        mut release: ReleaseLock,
        layout: LogLayout,
    ) -> Result<Self> {
        ensure!(
            release
                .services
                .iter()
                .filter(|service| service.enabled)
                .count()
                <= shared_types::MAX_SERVICES,
            "log catalog exceeds business service limit"
        );
        let builtin_orchestrator = !release
            .services
            .iter()
            .any(|service| service.service_id == "app-cli");
        inject_runtime_log_sources(&mut release, layout);
        inject_orchestrator_log_source(&mut release);
        let release_id = release.release_id.clone();
        let services = release
            .services
            .into_iter()
            .filter(|s| s.enabled)
            .map(|s| ServiceLogs {
                service_id: s.service_id,
                sources: s.logs,
            })
            .collect();
        let mut catalog = Self {
            schema_version: 1,
            app_id,
            source_root,
            log_root,
            layout,
            version: String::new(),
            release_id,
            builtin_orchestrator,
            services,
        };
        catalog.version = catalog.fingerprint()?;
        Ok(catalog)
    }

    fn fingerprint(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
            &self.app_id,
            &self.release_id,
            self.builtin_orchestrator,
            &self.source_root,
            &self.log_root,
            self.layout,
            &self.services,
        ))?)))
    }

    pub fn read(
        path: &Path,
        app_id: &str,
        source_root: &Path,
        allowed_roots: &[PathBuf],
    ) -> Result<Option<Self>> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("inspect log catalog"),
        };
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "log catalog must be a real file"
        );
        ensure!(
            metadata.len() <= MAX_CATALOG_BYTES,
            "log catalog exceeds size limit"
        );
        let file = std::fs::File::open(path).context("open log catalog")?;
        ensure!(
            crate::sources::file_identity(path, &metadata)
                == crate::sources::file_identity(path, &file.metadata()?),
            "log catalog changed while opening"
        );
        let mut bytes = Vec::new();
        file.take(MAX_CATALOG_BYTES + 1)
            .read_to_end(&mut bytes)
            .context("read log catalog")?;
        ensure!(
            bytes.len() as u64 <= MAX_CATALOG_BYTES,
            "log catalog exceeds size limit"
        );
        let catalog: Self = serde_json::from_slice(&bytes).context("decode log catalog")?;
        ensure!(
            catalog.schema_version == 1
                && catalog.app_id == app_id
                && same_root(&catalog.source_root, source_root),
            "log catalog project binding mismatch"
        );
        ensure!(
            allowed_roots
                .iter()
                .any(|root| same_root(&catalog.log_root, root)),
            "log catalog refers to an unapproved log root"
        );
        ensure!(
            catalog.version == catalog.fingerprint()?,
            "log catalog version does not match description"
        );
        ensure!(
            catalog.services.len()
                <= shared_types::MAX_SERVICES + usize::from(catalog.builtin_orchestrator),
            "log catalog exceeds business service limit"
        );
        if catalog.builtin_orchestrator {
            ensure!(
                catalog
                    .services
                    .iter()
                    .filter(|service| service.service_id == "app-cli")
                    .count()
                    == 1,
                "log catalog builtin orchestrator is missing or duplicated"
            );
        }
        for service in &catalog.services {
            workspace_manifest::validate_service_id(&service.service_id)
                .context("invalid catalog service id")?;
            ensure!(
                service.sources.len() <= shared_types::MAX_SOURCES,
                "log catalog exceeds source limit"
            );
        }
        Ok(Some(catalog))
    }

    /// Called only by the already-owned management path. Queries never publish.
    pub fn publish(&self, state_root: &Path) -> Result<()> {
        let path = catalog_path(state_root);
        let bytes = serde_json::to_vec(self)?;
        match std::fs::read(&path) {
            Ok(previous) if previous == bytes => return Ok(()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect previous log catalog"),
        }
        let mut temporary =
            tempfile::NamedTempFile::new_in(state_root).context("prepare log catalog")?;
        use std::io::Write;
        temporary.write_all(&bytes).context("write log catalog")?;
        temporary.as_file().sync_all().context("sync log catalog")?;
        temporary
            .persist(path)
            .map_err(|error| error.error)
            .context("publish log catalog")?;
        Ok(())
    }
}

fn same_root(left: &Path, right: &Path) -> bool {
    left == right
        || match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workspace_manifest::*;

    fn catalog(root: &Path) -> LogCatalog {
        catalog_for_count(root, 1).unwrap()
    }

    fn catalog_for_count(root: &Path, count: usize) -> Result<LogCatalog> {
        let mut service = crate::sources::orchestrator_service();
        service.service_id = "api".into();
        service.env.insert("SECRET".into(), "not-in-catalog".into());
        service.run.command = vec!["sensitive-command".into()];
        let release = ReleaseLock {
            schema_version: 1,
            release_id: "release".into(),
            workspace_name: "test".into(),
            minimum_app_cli_version: "0.3.9".into(),
            runtime_image_digest: "test".into(),
            pingap: LockedPingap {
                mode: PingapMode::Managed,
                version: "test".into(),
                commit: "test".into(),
                config: None,
            },
            services: (0..count)
                .map(|index| {
                    let mut service = service.clone();
                    service.service_id = format!("api-{index}");
                    // Application plus injected runtime reach the existing 128 business-source boundary.
                    service.logs[0].id = "application".into();
                    service
                })
                .collect(),
            bridge_service: None,
        };
        LogCatalog::from_release(
            "227".into(),
            root.join("source"),
            root.join("logs"),
            release,
            LogLayout::Builtin,
        )
    }

    #[tokio::test]
    async fn description_is_stable_and_contains_no_execution_authority_or_secrets() {
        let root = tempfile::tempdir().unwrap();
        let first = catalog(root.path());
        let second = catalog(root.path());
        assert_eq!(first.version, second.version);
        let mut replacement = second.clone();
        replacement.release_id = "next-release".into();
        assert_ne!(replacement.fingerprint().unwrap(), first.version);
        for count in [shared_types::MAX_SERVICES, shared_types::MAX_SERVICES + 1] {
            let result = catalog_for_count(root.path(), count);
            if count > shared_types::MAX_SERVICES {
                assert!(result.is_err(), "business service limit must not grow");
                continue;
            }
            let maximum = result.unwrap();
            assert_eq!(maximum.services.len(), count + 1);
            maximum.publish(root.path()).unwrap();
            let maximum = LogCatalog::read(
                &catalog_path(root.path()),
                "227",
                &root.path().join("source"),
                &[root.path().join("logs")],
            )
            .unwrap()
            .unwrap();
            let logs = crate::LogService::from_catalog(maximum);
            let sources = logs
                .sources(shared_types::LogQueryRequest::default())
                .await
                .unwrap();
            assert_eq!(sources.len(), count * 2 + 1);
            // Include the built-in service explicitly without consuming a business selector slot.
            let request = shared_types::LogQueryRequest {
                selectors: (0..count)
                    .map(|index| shared_types::LogSelector {
                        service_id: format!("api-{index}"),
                        source_ids: Vec::new(),
                    })
                    .chain(std::iter::once(shared_types::LogSelector {
                        service_id: "app-cli".into(),
                        source_ids: Vec::new(),
                    }))
                    .collect(),
                ..Default::default()
            };
            assert_eq!(logs.sources(request).await.unwrap().len(), count * 2 + 1);
        }
        let json = serde_json::to_string(&first).unwrap();
        assert!(!json.contains("not-in-catalog") && !json.contains("sensitive-command"));
        first.publish(root.path()).unwrap();
        let loaded = LogCatalog::read(
            &catalog_path(root.path()),
            "227",
            &root.path().join("source"),
            &[root.path().join("logs")],
        )
        .unwrap()
        .unwrap();
        assert_eq!(loaded.version, first.version);
        assert!(
            LogCatalog::read(
                &catalog_path(root.path()),
                "228",
                &root.path().join("source"),
                &[root.path().join("logs")]
            )
            .is_err()
        );
        assert!(
            LogCatalog::read(
                &catalog_path(root.path()),
                "227",
                &root.path().join("source"),
                &[root.path().join("other")]
            )
            .is_err()
        );
    }
}
