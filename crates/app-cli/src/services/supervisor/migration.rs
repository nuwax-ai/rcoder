//! Application-owned prestart scripts are advisory; physical cleanup is mandatory.
use super::*;
use std::{collections::BTreeMap, path::PathBuf};
use tokio::io::AsyncWriteExt;

mod process;
use process::{TransientDiagnostics, execute_transient};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MigrationFailureKind {
    Input,
    ExecutionLease,
    Spawn,
    Exit,
    Timeout,
    Cancelled,
    Wait,
    Output,
    Receipt,
    Cleanup,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MigrationFailure {
    pub kind: MigrationFailureKind,
    pub exit_code: Option<i32>,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MigrationOutcome {
    AlreadyCompleted,
    Completed,
    Advisory(MigrationFailure),
}

#[derive(Clone, Debug)]
pub(crate) struct MigrationReport {
    pub service_id: String,
    pub release_id: String,
    pub outcome: MigrationOutcome,
    pub stdout: String,
    pub stderr: String,
}

impl MigrationOutcome {
    fn advisory(kind: MigrationFailureKind, detail: impl Into<String>) -> Self {
        Self::Advisory(MigrationFailure {
            kind,
            exit_code: None,
            detail: detail.into(),
        })
    }
    fn add_diagnostic(&mut self, kind: MigrationFailureKind, detail: String) {
        match self {
            Self::Advisory(failure) => {
                failure.detail.push_str("; ");
                failure.detail.push_str(&detail);
            }
            _ => *self = Self::advisory(kind, detail),
        }
    }
}

#[derive(Clone)]
struct MigrationLogs {
    service: String,
    release: String,
    directory: PathBuf,
    secrets: Vec<String>,
}

impl MigrationLogs {
    fn new(
        spec: &ServiceSpec,
        release: &manifest::ReleaseLock,
        log_dir: &Path,
        env: &BTreeMap<String, String>,
    ) -> Self {
        Self {
            service: spec.service_id.clone(),
            release: release.release_id.clone(),
            directory: log_dir.join(&spec.service_id),
            secrets: runtime_secrets(env),
        }
    }
    fn redact(&self, text: &str) -> String {
        redact_output(text, &self.secrets)
    }
    fn emit(&self, stream: &str, text: &str) {
        for line in text.lines() {
            emit_event(&OrchestrationEvent::Log {
                service: self.service.clone(),
                line: format!("[migrate {stream}] {}", self.redact(line)),
            });
        }
    }
    fn path(&self, stream: &str) -> PathBuf {
        self.directory.join(format!("runtime.{stream}.log"))
    }
    async fn publish_remaining(&self, stream: &str, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let safe = self.redact(text);
        self.emit(stream, &safe);
        append_log(&self.path(stream), &safe).await
    }
    async fn finish(&self, mut execution: process::TransientReport) -> MigrationReport {
        for (stream, text) in [
            ("out", &execution.stdout_remaining),
            ("err", &execution.stderr_remaining),
        ] {
            if let Err(error) = self.publish_remaining(stream, text).await {
                execution.outcome.add_diagnostic(
                    MigrationFailureKind::Output,
                    format!("persist migration {stream} output: {error:#}"),
                );
            }
        }
        let stdout = self.redact(&execution.stdout);
        let stderr = self.redact(&execution.stderr);
        let mut outcome = execution.outcome;
        if let MigrationOutcome::Advisory(failure) = &mut outcome {
            failure.detail = self.redact(&failure.detail);
        }
        let summary = match &outcome {
            MigrationOutcome::AlreadyCompleted => {
                "INFO run.migrate already completed for this artifact".to_owned()
            }
            MigrationOutcome::Completed => "INFO run.migrate completed".to_owned(),
            MigrationOutcome::Advisory(failure) => {
                format!("ERROR run.migrate {:?}: {}", failure.kind, failure.detail)
            }
        };
        let summary = format!(
            "{summary}; service={} release={} phase=migration",
            self.service, self.release
        );
        if matches!(outcome, MigrationOutcome::Advisory(_)) {
            error!(service = %self.service, release_id = %self.release, phase = "migration", stdout = %stdout, stderr = %stderr, "{summary}");
        } else {
            info!(service = %self.service, release_id = %self.release, phase = "migration", stdout = %stdout, stderr = %stderr, "{summary}");
        }
        self.emit("result", &summary);
        if let Err(error) = append_log(&self.path("err"), &format!("{summary}\n")).await {
            let detail = self.redact(&format!(
                "persist migration result at {}: {error:#}",
                self.path("err").display()
            ));
            error!(service = %self.service, "{detail}");
            self.emit("result", &format!("ERROR {detail}"));
            outcome.add_diagnostic(MigrationFailureKind::Output, detail);
        }
        MigrationReport {
            service_id: self.service.clone(),
            release_id: self.release.clone(),
            outcome,
            stdout,
            stderr,
        }
    }
}

pub(crate) async fn run_migration_with_receipt_cancel(
    spec: &ServiceSpec,
    release: &manifest::ReleaseLock,
    workspace: &Path,
    log_dir: &Path,
    pg: Option<&shared_types::StartPgCredential>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<MigrationReport> {
    run_migration_with_receipt_and_timeout(
        spec,
        release,
        workspace,
        log_dir,
        pg,
        cancel,
        Duration::from_secs(300),
    )
    .await
}

pub(super) async fn run_migration_with_receipt_and_timeout(
    spec: &ServiceSpec,
    release: &manifest::ReleaseLock,
    workspace: &Path,
    log_dir: &Path,
    pg: Option<&shared_types::StartPgCredential>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    timeout: Duration,
) -> Result<MigrationReport> {
    // Credential/environment validation is independent of application migration policy.
    let environment = service_environment(spec, pg)?;
    let logs = MigrationLogs::new(spec, release, log_dir, &environment);
    if cancel.is_some_and(|token| token.is_cancelled()) {
        return Ok(logs
            .finish(process::TransientReport::advisory(
                MigrationFailureKind::Cancelled,
                "Migration cancelled before dispatch",
            ))
            .await);
    }
    let directory = workspace.join(&spec.dir);
    if let Err(error) = validate_transient_input(&spec.run.migrate, &directory) {
        return Ok(logs
            .finish(process::TransientReport::advisory(
                MigrationFailureKind::Input,
                format!("migration input: {error:#}"),
            ))
            .await);
    }
    let identity = crate::migration_journal::identity(release, &spec.service_id)?;
    let receipt = match crate::migration_journal::MigrationJournal::begin(workspace, identity) {
        Ok(Some(receipt)) => receipt,
        Ok(None) => {
            return Ok(logs
                .finish(process::TransientReport::empty(
                    MigrationOutcome::AlreadyCompleted,
                ))
                .await);
        }
        Err(error) => {
            // No lease means no script execution. Service startup remains allowed.
            return Ok(logs
                .finish(process::TransientReport::advisory(
                    MigrationFailureKind::ExecutionLease,
                    format!("migration execution skipped: {error:#}"),
                ))
                .await);
        }
    };
    let result = execute_transient(
        &spec.run.migrate,
        &directory,
        &environment,
        timeout,
        cancel,
        receipt,
        Some(logs.clone()),
    )
    .await;
    let (mut execution, receipt) = match result {
        Ok(result) => result,
        Err(error) => {
            if let Some(diagnostic) = error.downcast_ref::<TransientDiagnostics>() {
                logs.finish(diagnostic.0.clone()).await;
            }
            return Err(error);
        }
    };
    if execution.process_success
        && let Err(error) = receipt.complete()
    {
        execution.outcome.add_diagnostic(
            MigrationFailureKind::Receipt,
            format!("persist migration receipt: {error:#}"),
        );
    }
    Ok(logs.finish(execution).await)
}

// The generic transient helpers keep strict failure semantics. Only the
// application migration wrapper above turns a confirmed script failure advisory.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn run_transient_with_env(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    run_transient_with_environment_and_timeout(argv, cwd, env, Duration::from_secs(300)).await
}

#[cfg(all(test, unix))]
pub(super) async fn run_transient_with_timeout(
    argv: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<()> {
    run_transient_with_environment_and_timeout(argv, cwd, &Default::default(), timeout).await
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) async fn run_transient_with_environment_and_timeout(
    argv: &[String],
    cwd: &Path,
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<()> {
    let (execution, ()) = execute_transient(argv, cwd, env, timeout, None, (), None).await?;
    let secrets = runtime_secrets(env);
    let stdout = redact_output(&execution.stdout, &secrets);
    let stderr = redact_output(&execution.stderr, &secrets);
    match execution.outcome {
        MigrationOutcome::Completed => {
            info!(%stdout, %stderr, "transient command completed");
            Ok(())
        }
        MigrationOutcome::Advisory(failure) => {
            error!(%stdout, %stderr, "transient command failed");
            anyhow::bail!("{}", redact_output(&failure.detail, &secrets))
        }
        MigrationOutcome::AlreadyCompleted => anyhow::bail!("transient command was not executed"),
    }
}

fn runtime_secrets(env: &BTreeMap<String, String>) -> Vec<String> {
    let mut secrets = Vec::new();
    let inherited = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| !env.contains_key(key));
    for (key, value) in inherited.chain(env.iter().map(|(key, value)| (key.clone(), value.clone())))
    {
        let key = key.to_ascii_lowercase();
        if (key.contains("password")
            || key.contains("passwd")
            || key.contains("token")
            || key.contains("secret")
            || key.contains("api_key"))
            && !value.is_empty()
        {
            secrets.push(value.clone());
            if value.contains(['\n', '\r']) {
                // Streaming lines must not reveal pieces of a multiline
                // runtime secret before the full capture is available.
                secrets.extend(
                    value
                        .split(['\n', '\r'])
                        .filter(|part| !part.is_empty())
                        .map(str::to_owned),
                );
            }
            if let Ok(encoded) = serde_json::to_string(&value)
                && let Some(encoded) = encoded
                    .strip_prefix('"')
                    .and_then(|text| text.strip_suffix('"'))
            {
                secrets.push(encoded.to_owned());
            }
        }
        if let Ok(url) = reqwest::Url::parse(&value) {
            if let Some(password) = url.password().filter(|value| !value.is_empty()) {
                secrets.push(password.to_owned());
            }
            for (key, value) in url.query_pairs() {
                if key.eq_ignore_ascii_case("password") && !value.is_empty() {
                    secrets.push(value.into_owned());
                }
            }
        }
    }
    secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
    secrets.dedup();
    secrets
}

fn redact_output(text: &str, secrets: &[String]) -> String {
    let mut safe = text.to_owned();
    for secret in secrets {
        safe = safe.replace(secret, "[REDACTED]");
    }
    shared_types::redact_error_text(&safe)
}

async fn append_log(path: &Path, text: &str) -> Result<()> {
    let parent = path.parent().context("migration log has no parent")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("create migration log directory {}", parent.display()))?;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("open migration log {}", path.display()))?;
    file.write_all(text.as_bytes()).await?;
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

pub(super) fn validate_transient_input(argv: &[String], cwd: &Path) -> Result<()> {
    let program = argv.first().context("migration command is empty")?;
    anyhow::ensure!(!program.trim().is_empty(), "migration executable is empty");
    anyhow::ensure!(
        argv.iter().all(|argument| !argument.contains('\0')),
        "migration command contains a NUL byte"
    );
    let metadata = std::fs::metadata(cwd)
        .with_context(|| format!("inspect migration working directory {}", cwd.display()))?;
    anyhow::ensure!(
        metadata.is_dir(),
        "migration working directory is not a directory"
    );
    Ok(())
}
