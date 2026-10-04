//! Dev log observation is independent of app-cli process state and ownership.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use futures_util::future::BoxFuture;
use shared_types::{LogFormat, LogSource, SourceError};
use userapp_log_reader::{LogCatalog, LogLayout, LogProvider, LogService, catalog_path};

#[derive(Clone)]
pub struct DevLogProvider {
    config: file_server::Config,
    app_id: String,
    source_root: PathBuf,
    main_root: Option<PathBuf>,
    explicit_state_root: Option<std::ffi::OsString>,
}
impl DevLogProvider {
    pub fn new(config: &file_server::Config, app_id: &str) -> file_server::error::AppResult<Self> {
        Self::from_environment(config, app_id, |key| std::env::var_os(key))
    }

    pub(crate) fn from_environment(
        config: &file_server::Config,
        app_id: &str,
        lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> file_server::error::AppResult<Self> {
        let source_root = file_server::workspace::resolve_userapp_dev(app_id, None, config)?;
        let service = lookup("SERVICE_TYPE")
            .and_then(|value| value.into_string().ok())
            .and_then(|value| value.parse::<shared_types::ServiceType>().ok());
        let process_app = lookup("PROJECT_ID").and_then(|value| value.into_string().ok());
        if service == Some(shared_types::ServiceType::UserappBuilder) {
            let owner = process_app
                .as_deref()
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    file_server::error::AppError::validation("builder log PROJECT_ID is missing")
                })?;
            if owner != app_id {
                return Err(file_server::error::AppError::validation(
                    "log app_id does not match this builder's PROJECT_ID",
                ));
            }
        }
        let exclusive = process_app.as_deref() == Some(app_id)
            || config.userapp_single_app_id.as_deref() == Some(app_id);
        let main_root = exclusive.then(|| {
            lookup("APP_CLI_LOG_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/app/logs"))
        });
        let explicit_state_root = exclusive.then(|| lookup("APP_CLI_STATE_ROOT")).flatten();
        Ok(Self {
            config: config.clone(),
            app_id: app_id.to_owned(),
            source_root,
            main_root,
            explicit_state_root,
        })
    }

    fn load_blocking(&self) -> Result<LogService> {
        let fallback_root =
            file_server::service::dev_server::log::log_dir(&self.config, &self.app_id)
                .join("app-cli");
        let mut roots = vec![fallback_root.clone()];
        if let Some(root) = &self.main_root {
            if !roots.contains(root) {
                roots.push(root.clone());
            }
        }
        let primary_root = self
            .main_root
            .clone()
            .unwrap_or_else(|| fallback_root.clone());
        let mut diagnostics = Vec::new();
        let explicit = self.explicit_state_root.clone();
        let state_root = runtime_state_layout::resolve_state_root(
            &self.source_root,
            explicit.as_deref(),
            Some(std::ffi::OsStr::new(&self.app_id)),
        );
        let catalog = match state_root {
            Ok(Some(root)) => match LogCatalog::read(
                &catalog_path(&root),
                &self.app_id,
                &self.source_root,
                &roots,
            ) {
                Ok(catalog) => catalog,
                Err(error) => {
                    diagnostics.push(diagnostic("catalog_invalid", error));
                    None
                }
            },
            Ok(None) => None,
            Err(error) => {
                diagnostics.push(diagnostic("catalog_state_unreadable", error));
                None
            }
        };
        let legacy = catalog.is_none();
        let mut logs = match catalog {
            Some(catalog) => LogService::from_catalog(catalog),
            None => match read_legacy_release(&self.source_root) {
                Ok(Some(release)) => {
                    LogService::with_layout(release, primary_root.clone(), legacy_layout())
                }
                Ok(None) => LogService::idle(primary_root.clone()),
                Err(error) => {
                    diagnostics.push(diagnostic("release_lock_invalid", error));
                    LogService::idle(primary_root.clone())
                }
            },
        };
        for root in &roots {
            logs.add_writer_root(root);
        }
        if legacy {
            logs.add_legacy_runtime_roots(&roots);
            // Older writers may use either engine. These are exact contract paths,
            // not directory-name inference or recursive search.
            add_legacy_sources(&mut logs, &self.source_root, &roots, &mut diagnostics)?;
        }
        let orchestrator = log_source("orchestrator", "app-cli.log.*", LogFormat::Jsonl);
        for root in &roots {
            logs.add_directory("app-cli", orchestrator.clone(), root.clone())?;
        }
        // Supervisord captures wrapper/CLI failures before tracing is installed.
        // A global wrapper log is readable only by the app owning this process.
        if let Some(root) = &self.main_root {
            logs.add_directory(
                "app-cli",
                log_source(
                    "management-launch",
                    "rcoder-app-runtime.log*",
                    LogFormat::Text,
                ),
                root.clone(),
            )?;
        }
        logs.add_directory(
            "app-cli",
            log_source("owner-recovery", "owner-recovery.log", LogFormat::Text),
            fallback_root.clone(),
        )?;
        let dev_logs = file_server::service::dev_server::log::log_dir(&self.config, &self.app_id);
        logs.add_directory(
            "app-cli",
            log_source("dev-server", "dev-*.log", LogFormat::Text),
            dev_logs,
        )?;
        // Exact known build layout only: source/logs/<service>/dev-*.log.
        // It is read-only and bounded to the contract's maximum service count.
        match read_build_directories(&self.source_root.join("logs")) {
            Ok(directories) => {
                for (service_id, directory) in directories {
                    logs.add_directory(
                        &service_id,
                        log_source("build", "dev-*.log", LogFormat::Text),
                        directory,
                    )?;
                }
            }
            Err(error) => diagnostics.push(diagnostic("build_logs_unreadable", error)),
        }
        for diagnostic in diagnostics {
            logs.add_diagnostic(diagnostic);
        }
        Ok(logs)
    }
}
impl LogProvider for DevLogProvider {
    fn load(&self) -> BoxFuture<'_, Result<LogService>> {
        let provider = self.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || provider.load_blocking())
                .await
                .context("join dev log catalog observation")?
        })
    }
}

fn read_legacy_release(source: &Path) -> Result<Option<shared_types::ReleaseLock>> {
    let path = source.join("release.lock.toml");
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect log service release description"),
    };
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "log release description must be a real file"
    );
    anyhow::ensure!(
        metadata.len() <= 1024 * 1024,
        "log release description exceeds size limit"
    );
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .context("open log service release description")?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .context("read log service release description")?;
    anyhow::ensure!(
        bytes.len() <= 1024 * 1024,
        "log release description exceeds size limit"
    );
    let text = std::str::from_utf8(&bytes).context("release description is not UTF-8")?;
    Ok(Some(
        shared_types::load_release_lock(text).context("parse log service release description")?,
    ))
}
fn legacy_layout() -> LogLayout {
    // app-runtime managed path uses the supervisord service writer; local CLI uses builtin.
    match std::env::var("APP_CLI_SUPERVISOR_SOCKET") {
        Ok(url) if !url.is_empty() => LogLayout::Supervisord,
        _ => LogLayout::Builtin,
    }
}
fn read_build_directories(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("inspect build log directory"),
    };
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "build log root must be a real directory"
    );
    let mut directories = Vec::new();
    for entry in std::fs::read_dir(root).context("read build log directory")? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if !kind.is_dir() || kind.is_symlink() {
            continue;
        }
        let service_id = entry.file_name().to_string_lossy().into_owned();
        shared_types::validate_service_id(&service_id).context("invalid build log service id")?;
        anyhow::ensure!(
            directories.len() < shared_types::MAX_SERVICES,
            "build logs exceed service limit"
        );
        directories.push((service_id, entry.path()));
    }
    directories.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(directories)
}
fn diagnostic(code: &str, error: anyhow::Error) -> SourceError {
    SourceError {
        service_id: "workspace".into(),
        source_id: "catalog".into(),
        code: code.into(),
        message: format!("{error:#}"),
    }
}
fn log_source(id: &str, pattern: &str, format: LogFormat) -> LogSource {
    LogSource {
        id: id.into(),
        glob: pattern.into(),
        format,
        multiline_start_pattern: None,
    }
}

fn add_legacy_sources(
    logs: &mut LogService,
    source: &Path,
    roots: &[PathBuf],
    diagnostics: &mut Vec<SourceError>,
) -> Result<()> {
    let report = match shared_types::discover_projects_report(source) {
        Ok(report) => report,
        Err(error) => {
            diagnostics.push(diagnostic(
                "project_description_unreadable",
                anyhow::anyhow!("{error}"),
            ));
            return Ok(());
        }
    };
    for issue in report.diagnostics {
        diagnostics.push(diagnostic(
            "project_description_invalid",
            anyhow::anyhow!("{}", issue.issue),
        ));
    }
    for project in report.projects {
        let service_id = project.service_id();
        for declared in &project.manifest.logs.sources {
            for root in roots {
                logs.add_directory(service_id, declared.clone(), root.join(service_id))?;
            }
        }
        if project.manifest.project.r#type != shared_types::ProjectType::Static
            && !project
                .manifest
                .logs
                .sources
                .iter()
                .any(|source| source.id == "runtime")
        {
            let pattern = format!("{{runtime.*.log,{service_id}.log}}");
            logs.replace_pattern(service_id, "runtime", pattern.clone());
            let runtime = log_source("runtime", &pattern, LogFormat::Text);
            for root in roots {
                logs.add_directory(service_id, runtime.clone(), root.join(service_id))?;
                logs.add_directory(service_id, runtime.clone(), root.join("services"))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::LogQueryRequest;

    fn environment(root: &Path, service_type: &str) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        let values = std::collections::BTreeMap::from([
            ("SERVICE_TYPE", std::ffi::OsString::from(service_type)),
            ("APP_CLI_MANAGED", std::ffi::OsString::from("1")),
            ("PROJECT_ID", std::ffi::OsString::from("227")),
            (
                "APP_CLI_LOG_DIR",
                root.join("current-app-logs").into_os_string(),
            ),
            (
                "APP_CLI_STATE_ROOT",
                root.join("state/227").into_os_string(),
            ),
        ]);
        move |key| values.get(key).cloned()
    }

    #[tokio::test]
    async fn builder_platform_spelling_rejects_foreign_app_before_any_log_read() {
        let root = tempfile::tempdir().unwrap();
        let config = file_server::Config {
            userapp_workspace_dir: root.path().join("source"),
            log_base_dir: root.path().join("logs"),
            ..Default::default()
        };
        for service_type in ["user-app-builder", "userapp-builder"] {
            let error = DevLogProvider::from_environment(
                &config,
                "228",
                environment(root.path(), service_type),
            )
            .err()
            .expect("foreign app rejected");
            assert!(error.to_string().contains("PROJECT_ID"));
        }
        assert!(!root.path().join("state").exists());
    }

    #[tokio::test]
    async fn correct_builder_can_read_orchestrator_logs_when_source_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let logs = root.path().join("current-app-logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("app-cli.log.2026-10-04"),
            "{\"message\":\"manifest missing at source root\"}\n",
        )
        .unwrap();
        std::fs::write(
            logs.join("rcoder-app-runtime.log"),
            "project origin mismatch before tracing initialization\n",
        )
        .unwrap();
        let config = file_server::Config {
            userapp_workspace_dir: root.path().join("missing-source"),
            log_base_dir: root.path().join("logs"),
            ..Default::default()
        };
        let provider = DevLogProvider::from_environment(
            &config,
            "227",
            environment(root.path(), "user-app-builder"),
        )
        .unwrap();
        let response = provider
            .load()
            .await
            .unwrap()
            .query(LogQueryRequest::default())
            .await
            .unwrap();
        assert!(
            response
                .logs
                .iter()
                .any(|log| log.message == "manifest missing at source root")
        );
        assert!(
            response
                .source_errors
                .iter()
                .any(|error| error.code == "project_description_unreadable")
        );
        assert!(
            response
                .logs
                .iter()
                .any(|log| log.source_id == "management-launch"
                    && log.message.contains("before tracing initialization"))
        );
        let request = LogQueryRequest {
            selectors: vec![shared_types::LogSelector {
                service_id: "app-cli".into(),
                source_ids: vec!["management-launch".into()],
            }],
            ..Default::default()
        };
        let sources = provider
            .load()
            .await
            .unwrap()
            .sources(request.clone())
            .await
            .unwrap();
        assert!(
            sources
                .iter()
                .any(|source| source.source_id == "management-launch"
                    && source.matched_files == ["rcoder-app-runtime.log"])
        );
        use futures_util::StreamExt;
        let mut stream = userapp_log_reader::stream(std::sync::Arc::new(provider), request);
        let event = stream.next().await.unwrap();
        assert!(
            matches!(event, userapp_log_reader::LogStreamEvent::Log(log) if log.source_id == "management-launch" && log.message.contains("before tracing initialization"))
        );
        drop(stream);
        assert!(!config.userapp_workspace_dir.exists());
        assert!(!root.path().join("state").exists());
    }

    #[tokio::test]
    async fn multi_app_server_does_not_read_unbound_global_cli_log_root() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("app-cli.log.2026-10-04"),
            "{\"message\":\"OTHER_APP_SECRET\"}\n",
        )
        .unwrap();
        let config = file_server::Config {
            userapp_workspace_dir: root.path().join("source"),
            log_base_dir: root.path().join("logs"),
            ..Default::default()
        };
        let provider = DevLogProvider::from_environment(&config, "227", |key| {
            (key == "APP_CLI_LOG_DIR").then(|| global.clone().into_os_string())
        })
        .unwrap();
        let response = provider
            .load()
            .await
            .unwrap()
            .query(LogQueryRequest::default())
            .await
            .unwrap();
        assert!(response.logs.is_empty());
        assert!(!root.path().join("state").exists());
    }
}
