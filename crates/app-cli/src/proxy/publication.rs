//! app-cli owned publication transactions for unmodified official Pingap.
//! Configuration markers prove listener execution; process ownership comes
//! exclusively from the host, never from a private Pingap application API.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use pingap_config::PingapConfig;
use tokio::time::Instant;

use super::{
    admin_probe::{self, AdminEndpoint},
    apply_status::ConfirmedPublication,
    compiler::{self, CompileOutcome},
};

#[cfg(all(test, unix))]
#[path = "publication_budget_tests.rs"]
mod budget_tests;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessInstance {
    pub pid: u32,
    /// Host-issued incarnation. It is not a Pingap-supplied nonce.
    pub id: String,
}

/// Hosts must retain the exact process identity across every await. Stop must
/// confirm the owned execution tree is empty before start may replace it.
#[async_trait::async_trait]
pub(crate) trait ProcessHost: Send {
    async fn stop_exact(&mut self, deadline: Instant) -> Result<()>;
    async fn start_exact(
        &mut self,
        candidate: &CompileOutcome,
        deadline: Instant,
    ) -> Result<ProcessInstance>;
    async fn verify_exact(&mut self, expected: &ProcessInstance, deadline: Instant) -> Result<()>;
    async fn confirm(
        &mut self,
        candidate: &CompileOutcome,
        instance: &ProcessInstance,
        deadline: Instant,
    ) -> Result<ConfirmedPublication>;
    async fn commit(
        &mut self,
        candidate: &CompileOutcome,
        receipt: &ConfirmedPublication,
        deadline: Instant,
    ) -> Result<()>;
    fn mark_unknown(&self, message: String) -> anyhow::Error;
}

/// One shared transition for both native Child and supervisord hosts. The
/// original request remains failed even when the last verified snapshot is
/// restored. An absent snapshot does not turn confirmed cleanup into Unknown.
pub(crate) async fn restart_verified<H: ProcessHost>(
    host: &mut H,
    candidate: &CompileOutcome,
    previous: Option<&CompileOutcome>,
    deadline: Instant,
) -> Result<ConfirmedPublication> {
    validate_immutable_until(candidate, deadline).await?;
    compiler::record_official_phase(candidate, shared_types::ProxyPhase::Preparing, None)?;
    ensure!(
        Instant::now() < deadline,
        "proxy replacement expired before stopping the old instance"
    );
    // Failure here leaves the original Child/receipt retained. Never bypass an
    // uncertain stop by spawning a second instance or by using another port.
    compiler::record_official_phase(candidate, shared_types::ProxyPhase::Stopping, None)?;
    host.stop_exact(deadline).await.map_err(|error| {
        protected_error(
            host,
            candidate,
            format!("proxy old instance cleanup unconfirmed: {error:#}"),
        )
    })?;
    let forward_deadline = confirmation_deadline(deadline)?;
    match start_confirm_commit(host, candidate, forward_deadline).await {
        Ok(receipt) => Ok(receipt),
        Err(original) => {
            log_transition_failure(candidate, "candidate", &original);
            host.stop_exact(deadline).await.map_err(|cleanup| protected_error(host,candidate,format!("proxy candidate rejected: {original:#}; candidate cleanup unconfirmed: {cleanup:#}")))?;
            let Some(previous) = previous else {
                compiler::record_official_phase(candidate, shared_types::ProxyPhase::Failed, None)?;
                return Err(original).context(
                    "proxy candidate rejected; cleanup confirmed; a new request may retry",
                );
            };
            let restore = async {
                let restore_deadline = confirmation_deadline(deadline)?;
                validate_immutable_until(previous, restore_deadline).await?;
                start_confirm_commit(host, previous, restore_deadline).await
            }
            .await;
            match restore {
                Ok(_) => {
                    tracing::warn!(operation_id=%candidate.operation_id.as_deref().unwrap_or(&candidate.publication_id),
                        phase="restored", "proxy replacement failed; previous verified snapshot restored; original request remains failed");
                    // Current serving confirmation remains the restored snapshot;
                    // the failed request remains visible in its operation record.
                    Err(original).context(
                        "proxy replacement failed; previous verified configuration restored",
                    )
                }
                Err(restore_error) => {
                    log_transition_failure(candidate, "restore", &restore_error);
                    // Restoration failure is still retryable if its new tree is
                    // known empty. Only physical uncertainty protects writes.
                    host.stop_exact(deadline).await.map_err(|cleanup| protected_error(host,candidate,format!("proxy candidate rejected: {original:#}; restoration failed: {restore_error:#}; cleanup unconfirmed: {cleanup:#}")))?;
                    compiler::record_official_phase(
                        candidate,
                        shared_types::ProxyPhase::Failed,
                        None,
                    )?;
                    Err(original).context(format!("proxy restoration failed but cleanup confirmed: {restore_error:#}; a new request may retry"))
                }
            }
        }
    }
}

/// Every launched attempt may need exact tree cleanup, including cold startup
/// and a failed restoration. Keep the existing eight-second settlement reserve
/// (three-second grace plus tree reaping) inside the caller's total budget.
/// A shorter caller budget splits remaining time rather than discarding all
/// opportunity to confirm; genuinely late cleanup remains protected Unknown.
fn confirmation_deadline(deadline: Instant) -> Result<Instant> {
    let now = Instant::now();
    let available = deadline.saturating_duration_since(now);
    ensure!(
        !available.is_zero(),
        "proxy replacement deadline exhausted before process activation"
    );
    let reserve = (available / 2).min(std::time::Duration::from_secs(8));
    deadline
        .checked_sub(reserve)
        .context("proxy confirmation deadline is outside the clock range")
}

fn log_transition_failure(candidate: &CompileOutcome, phase: &'static str, error: &anyhow::Error) {
    let detail = shared_types::sanitize_error_text(&format!("{error:#}"));
    tracing::warn!(operation_id=%candidate.operation_id.as_deref().unwrap_or(&candidate.publication_id),
        phase,error=%detail,"official proxy transition failed");
    crate::orchestration_events::emit(&crate::orchestration_events::OrchestrationEvent::Log {
        service: "pingap".into(),
        line: format!("Proxy {phase} failed: {detail}"),
    });
}

fn protected_error<H: ProcessHost>(
    host: &H,
    candidate: &CompileOutcome,
    message: String,
) -> anyhow::Error {
    tracing::error!(operation_id=%candidate.operation_id.as_deref().unwrap_or(&candidate.publication_id),
        phase="recovery_required",error=%shared_types::sanitize_error_text(&message),"proxy physical cleanup remains unconfirmed");
    match compiler::record_official_phase(
        candidate,
        shared_types::ProxyPhase::RecoveryRequired,
        None,
    ) {
        Ok(()) => host.mark_unknown(message),
        Err(error) => host.mark_unknown(format!(
            "{message}; persist recovery phase failed: {error:#}"
        )),
    }
}

async fn start_confirm_commit<H: ProcessHost>(
    host: &mut H,
    candidate: &CompileOutcome,
    deadline: Instant,
) -> Result<ConfirmedPublication> {
    ensure!(
        Instant::now() < deadline,
        "proxy replacement expired before process start"
    );
    compiler::record_official_phase(candidate, shared_types::ProxyPhase::Starting, None)?;
    let instance = host.start_exact(candidate, deadline).await?;
    host.verify_exact(&instance, deadline).await?;
    compiler::record_official_phase(
        candidate,
        shared_types::ProxyPhase::Verifying,
        Some(&instance.id),
    )?;
    let receipt = host.confirm(candidate, &instance, deadline).await?;
    ensure!(
        receipt.process_id == instance.pid && receipt.instance_id == instance.id,
        "proxy observation does not belong to the retained host instance"
    );
    host.verify_exact(&instance, deadline).await?;
    ensure!(
        Instant::now() < deadline,
        "proxy confirmation completed after its deadline"
    );
    host.commit(candidate, &receipt, deadline).await?;
    host.verify_exact(&instance, deadline).await?;
    ensure!(
        Instant::now() < deadline,
        "proxy commit exceeded its total deadline"
    );
    let phase = if !candidate.entry_probes.is_empty()
        && candidate
            .entry_probes
            .iter()
            .all(|probe| probe.expected_status == 503)
    {
        shared_types::ProxyPhase::Standby
    } else {
        shared_types::ProxyPhase::Serving
    };
    compiler::record_official_phase(candidate, phase, Some(&instance.id))?;
    Ok(receipt)
}

pub(crate) async fn validate_immutable_until(
    candidate: &CompileOutcome,
    deadline: Instant,
) -> Result<Vec<u8>> {
    tokio::time::timeout_at(deadline, validate_immutable(candidate))
        .await
        .context("proxy immutable snapshot observation deadline exhausted")?
}

pub(crate) async fn validate_immutable(candidate: &CompileOutcome) -> Result<Vec<u8>> {
    let metadata = tokio::fs::symlink_metadata(&candidate.config_path)
        .await
        .context("observe immutable proxy snapshot")?;
    ensure!(
        metadata.file_type().is_file(),
        "proxy snapshot is not a regular file"
    );
    let bytes = tokio::fs::read(&candidate.config_path)
        .await
        .context("read immutable proxy snapshot")?;
    let config = PingapConfig::new(&bytes, true).context("decode proxy snapshot")?;
    ensure!(
        compiler::configuration_digest(&config)? == candidate.config_digest
            && admin_probe::hashes_match(&config.hash()?, &candidate.expected_hash),
        "proxy snapshot differs from its authorized configuration"
    );
    Ok(bytes)
}

/// Never launch Pingap against the mutable active file. It remains a diagnostic
/// view and a topology source for legacy upgrades, not a reload control plane.
pub(crate) fn freeze_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .context("immutable proxy snapshot parent missing")?;
    process_utils::command_context::create_durable_directory(parent)?;
    ensure!(
        std::fs::canonicalize(parent)? == parent,
        "immutable proxy directory resolves outside the owner range"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut options = std::fs::File::options();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file.flush()?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                std::fs::symlink_metadata(path)?.file_type().is_file(),
                "immutable proxy snapshot is not a regular file"
            );
            ensure!(
                std::fs::read(path)? == bytes,
                "publication identity already belongs to different immutable content"
            );
        }
        Err(error) => return Err(error).context("create immutable proxy snapshot"),
    }
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(crate) async fn freeze_candidate(
    root: &Path,
    outcome: &CompileOutcome,
) -> Result<CompileOutcome> {
    let bytes = validate_immutable(outcome).await?;
    ensure!(
        uuid::Uuid::parse_str(&outcome.publication_id).is_ok(),
        "invalid proxy operation identity"
    );
    let path = root
        .join("official-snapshots")
        .join(format!("{}.toml", outcome.publication_id));
    freeze_private(&path, &bytes)?;
    Ok(CompileOutcome {
        config_path: path,
        ..outcome.clone()
    })
}

pub(crate) async fn standby_candidate(root: &Path, topology: &Path) -> Result<CompileOutcome> {
    let content = match tokio::fs::read_to_string(topology).await {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("read standby topology"),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let (content, _) = compiler::build_standby_from_topology(&id, content.as_deref())?;
    let config = PingapConfig::new(content.as_bytes(), true)?;
    let outcome = compiler::persist_candidate(root, &config, &id, 503).await?;
    freeze_candidate(root, &outcome).await
}

pub async fn publish_standby_confirmed(
    root: &Path,
    endpoint: &AdminEndpoint,
) -> Result<CompileOutcome> {
    publish_standby_confirmed_to_path(root, &compiler::active_config_path(root), endpoint).await
}
pub async fn publish_standby_confirmed_to_path(
    root: &Path,
    target: &Path,
    endpoint: &AdminEndpoint,
) -> Result<CompileOutcome> {
    publish_standby_confirmed_to_path_until(
        root,
        target,
        endpoint,
        Instant::now() + admin_probe::CONFIRM_BUDGET,
    )
    .await
}
pub async fn publish_standby_confirmed_to_path_until(
    root: &Path,
    target: &Path,
    _endpoint: &AdminEndpoint,
    deadline: Instant,
) -> Result<CompileOutcome> {
    let outcome = tokio::time::timeout_at(deadline, standby_candidate(root, target))
        .await
        .context("standby preparation exceeded proxy operation deadline")??;
    let host = crate::services::supervisord_host::SupervisordHost::from_env_until(deadline)
        .await?
        .context("proxy replacement requires a managed process host")?;
    host.replace_official(root, &outcome, deadline).await?;
    Ok(outcome)
}

pub async fn publish_confirmed(
    root: &Path,
    _target: &Path,
    candidate: &CompileOutcome,
    _endpoint: &AdminEndpoint,
) -> Result<ConfirmedPublication> {
    let deadline = Instant::now() + admin_probe::CONFIRM_BUDGET;
    let host = crate::services::supervisord_host::SupervisordHost::from_env_until(deadline)
        .await?
        .context("proxy replacement requires a managed process host")?;
    host.replace_official(root, candidate, deadline).await
}
