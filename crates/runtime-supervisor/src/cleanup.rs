//! Shared process cleanup after an observed worker exit. No business operation
//! is replayed, and unknown command/migration outcomes remain unchanged.
use crate::record::{self, AbandonedGeneration, Generation, GenerationPhase};
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

/// A trusted CLI-provided adapter, not an executable loaded from a receipt.
/// Offline callers must supply the original CLI's adapter when adopting work.
#[derive(Clone, Debug)]
pub struct CleanupCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
}

pub(crate) async fn abandoned(
    mut target: AbandonedGeneration,
    adapter: Option<&CleanupCommand>,
) -> Result<()> {
    let result = once(&target.root, &mut target.value, adapter).await;
    if let Err(error) = &result {
        record_failure(&target.root, &mut target.value, error);
    }
    result
}

/// Caller retains the generation lock and has positive worker-exit evidence.
pub(crate) async fn once(
    root: &Path,
    value: &mut Generation,
    adapter: Option<&CleanupCommand>,
) -> Result<()> {
    process_utils::command_authority::Gate::try_acquire(root)?.close()?;
    value.phase = GenerationPhase::Draining;
    record::save(&root.join("generation.json"), value)?;
    process_utils::guardian::recover(root)?;
    settle_unspawned(root)?;
    process_utils::command_context::require_quiescent(&root.join("commands"))?;
    if let Some(adapter) = adapter {
        // A callback may survive a killed guardian. Let it finish before
        // dispatching another one; its guard covers the entire engine RPC.
        drop(record::lock(&root.join("external-cleanup.lock"))?);
        let mut command = tokio::process::Command::new(&adapter.program);
        command
            .args(&adapter.args)
            .current_dir(&adapter.cwd)
            .stdin(Stdio::null());
        process_utils::command_authority::detach_command(&mut command);
        command
            .env_remove(crate::WORKER_ENV)
            .env_remove(crate::TOKEN_ENV)
            .env("RCODER_SUPERVISOR_CLEANUP_ROOT", root)
            .env("RCODER_SUPERVISOR_CLEANUP_TOKEN", &value.token)
            .kill_on_drop(true);
        // This is the existing external-engine RPC budget, not worker TERM
        // grace. Interactive worker grace remains three seconds in app-cli.
        let output = tokio::time::timeout(Duration::from_secs(30), command.output())
            .await
            .context("external engine cleanup timed out")??;
        ensure!(
            output.status.success(),
            "external engine cleanup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let _callback_guard = record::lock(&root.join("external-cleanup.lock"))?;
    value.phase = GenerationPhase::Quiescent;
    if let Err(error) = record::save(&root.join("generation.json"), value) {
        value.phase = GenerationPhase::Draining;
        return Err(error).context("persist generation cleanup completion");
    }
    Ok(())
}

pub(crate) fn record_failure(root: &Path, value: &mut Generation, error: &anyhow::Error) {
    value.phase = GenerationPhase::Draining;
    let message = format!("command cleanup pending: {error:#}");
    if value.error.as_deref() != Some(&message) {
        value.error = Some(message);
        if let Err(save_error) = record::save(&root.join("generation.json"), value) {
            tracing::warn!(%save_error, "could not persist cleanup pending state");
        }
    }
}

fn settle_unspawned(root: &Path) -> Result<()> {
    let commands = root.join("commands");
    if !commands.try_exists()? {
        return Ok(());
    }
    for entry in std::fs::read_dir(commands)? {
        let path = entry?.path();
        let mut value: serde_json::Value = record::read(&path)?;
        ensure!(value["version"] == 1, "unknown command record version");
        if value["phase"] == "SpawnPending" {
            value["phase"] = "Quiescent".into();
            value["termination"] = "OwnerExitedBeforeSpawn".into();
            record::save(&path, &value)?;
        }
    }
    Ok(())
}
