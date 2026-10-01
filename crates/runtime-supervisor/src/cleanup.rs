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

/// Structured physical-cleanup outcome (recovery v2 plan §7.1). This is
/// process-scope evidence only; business command results stay separate and
/// are never manufactured from a cleanup.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CleanupOutcome {
    /// The managed scope is verified empty; authorization closed.
    Empty,
    /// Managed processes are still winding down within a bounded window.
    Stopping { detail: String },
    /// The execution backend could not be observed; cleanup unconfirmed with
    /// the concrete reason.
    ObservationFailed { reason: String },
    /// Records belong to another management domain and are retained as
    /// history rather than cleaned here.
    ForeignIdentity { detail: String },
}

impl CleanupOutcome {
    fn path(root: &Path) -> PathBuf {
        root.join("cleanup-outcome.json")
    }
    /// Persist the structured outcome beside the generation receipts. The
    /// sidecar file is additive: older binaries ignore it, and the generation
    /// schema stays untouched (no downgrade breakage).
    pub fn record(self, root: &Path) -> Result<()> {
        let path = Self::path(root);
        let bytes = serde_json::to_vec(&self)?;
        let mut file = tempfile::NamedTempFile::new_in(root)?;
        std::io::Write::write_all(&mut file, &bytes)?;
        std::io::Write::flush(&mut file)?;
        file.as_file().sync_all()?;
        process_utils::atomic_file::persist(file, &path)
            .with_context(|| format!("publish cleanup outcome {}", path.display()))?;
        #[cfg(unix)]
        std::fs::File::open(root)?.sync_all()?;
        Ok(())
    }
    /// Read the recorded outcome, if any.
    /// Read the recorded outcome for the CURRENT attempt. `Ok(None)` = no
    /// record（首次尝试）；读取/解码失败如实上报为 `Err`——调用方据此
    /// 区分"无分类"与"分类损坏"，避免把上一次尝试的旧原因当成本次的。
    pub fn recorded(root: &Path) -> Result<Option<Self>> {
        let bytes = match std::fs::read(Self::path(root)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).context("read cleanup outcome record");
            }
        };
        serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "decode cleanup outcome record at {}",
                Self::path(root).display()
            )
        })
    }
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
    // recovery v2 §7.1：死守护进程的 Running 命令按"范围已灭、结果未知"
    // 收束（运行授权关闭，不伪造业务结果）；活命令如实 Stopping 重试。
    match process_utils::guardian::recover_with_scope_check(root)? {
        process_utils::guardian::RecoveryOutcome::Settled => {}
        process_utils::guardian::RecoveryOutcome::Stopping { detail } => {
            CleanupOutcome::Stopping {
                detail: detail.clone(),
            }
            .record(root)?;
            anyhow::bail!("owned command still draining: {detail}");
        }
    }
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
        if !output.status.success() {
            // Prefer the adapter's own structured classification when it
            // recorded one (ForeignIdentity 等)，不回写成 ObservationFailed；
            // 适配器未分类时才落默认 ObservationFailed。
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            match CleanupOutcome::recorded(root) {
                // 适配器已分类（非 Empty）：原样保留本次尝试的分类。
                Ok(Some(
                    already @ (CleanupOutcome::ObservationFailed { .. }
                    | CleanupOutcome::ForeignIdentity { .. }
                    | CleanupOutcome::Stopping { .. }),
                )) => {
                    let detail = format!("{already:?}");
                    already.record(root)?;
                    tracing::debug!(
                        %detail, %stderr,
                        "engine cleanup failed with adapter classification"
                    );
                }
                // Empty 是上一次成功尝试的残留（本次失败说明记录早于本次
                // 失败——不覆盖证据，落本次 ObservationFailed）。
                Ok(Some(CleanupOutcome::Empty)) | Ok(None) => {
                    CleanupOutcome::ObservationFailed {
                        reason: format!("engine cleanup failed: {stderr}"),
                    }
                    .record(root)?;
                }
                // 记录读取/解码失败：不沿用旧原因，落显式读取错误。
                Err(record_error) => {
                    CleanupOutcome::ObservationFailed {
                        reason: format!(
                            "engine cleanup failed: {stderr}; prior outcome \
                             record unreadable: {record_error:#}"
                        ),
                    }
                    .record(root)?;
                }
            }
            anyhow::bail!("external engine cleanup failed: {stderr}");
        }
    }
    let _callback_guard = record::lock(&root.join("external-cleanup.lock"))?;
    value.phase = GenerationPhase::Quiescent;
    if let Err(error) = record::save(&root.join("generation.json"), value) {
        value.phase = GenerationPhase::Draining;
        return Err(error).context("persist generation cleanup completion");
    }
    CleanupOutcome::Empty.record(root)?;
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
