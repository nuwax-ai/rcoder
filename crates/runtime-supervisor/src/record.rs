use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    #[default]
    Run,
    /// Recreate the management plane, with automatic business startup suppressed.
    Stopped,
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Generation {
    pub version: u32,
    pub id: String,
    pub supervisor: String,
    pub token: String,
    pub intent: Intent,
    pub phase: GenerationPhase,
    /// Launch handshake and diagnostics only. Signals use the retained Child.
    #[serde(default)]
    pub worker_pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum GenerationPhase {
    Pending,
    Running,
    Draining,
    Quiescent,
    Revoked,
}

pub(crate) fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("receipt parent missing")?;
    process_utils::command_context::create_durable_directory(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    file.as_file().sync_all()?;
    process_utils::atomic_file::persist(file, path)
        .with_context(|| format!("publish receipt {}", path.display()))?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
pub(crate) fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read receipt {}", path.display()))?,
    )
    .with_context(|| format!("decode receipt {}", path.display()))
}
pub(crate) fn lock(path: &Path) -> Result<File> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock().context("execution scope is still owned")?;
    Ok(file)
}
pub(crate) fn is_locked(path: &Path) -> Result<()> {
    let file = File::options().read(true).write(true).open(path)?;
    match file.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => Ok(()),
        Err(error) => Err(error.into()),
        Ok(()) => anyhow::bail!("execution owner has exited"),
    }
}
pub(crate) fn generation(root: &Path) -> Result<Generation> {
    let value: Generation = read(&root.join("generation.json"))?;
    ensure!(
        value.version == 1 && root.file_name().and_then(|s| s.to_str()) == Some(&value.id),
        "generation identity/version mismatch"
    );
    Ok(value)
}
pub(crate) fn work_root(scope: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(id).is_ok(),
        "invalid execution generation"
    );
    Ok(scope.join("work").join(id))
}

/// An identity-bound local cleanup receipt, not business success evidence.
#[derive(Clone, Debug)]
pub struct Quiescence {
    pub generation: String,
    pub supervisor_id: String,
}

pub fn verify_quiescent(scope: &Path, id: &str) -> Result<Quiescence> {
    let root = work_root(scope, id)?;
    let _lock = lock(&root.join("generation.lock"))?;
    let record = generation(&root)?;
    ensure!(
        matches!(
            record.phase,
            GenerationPhase::Quiescent | GenerationPhase::Revoked
        ),
        "execution generation has no cleanup receipt"
    );
    process_utils::guardian::recover(&root)?;
    process_utils::command_context::require_quiescent(&root.join("commands"))?;
    Ok(Quiescence {
        generation: record.id,
        supervisor_id: record.supervisor,
    })
}

pub fn verify_live(scope: &Path, supervisor: &str, id: &str) -> Result<()> {
    let root = work_root(scope, id)?;
    let generation = generation(&root)?;
    ensure!(
        generation.supervisor == supervisor && generation.phase == GenerationPhase::Running,
        "execution identity is not active"
    );
    is_locked(&root.join("generation.lock"))?;
    is_locked(&scope.join("owner.lock"))?;
    Ok(())
}

/// Run only with the stable owner lock held. A late root guardian must obtain
/// this same generation lock and cannot consume revoked authorization.
pub(crate) fn reconcile(scope: &Path) -> Result<()> {
    let dir = scope.join("work");
    if !dir.try_exists()? {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let root = entry?.path();
        if !root.join("generation.json").try_exists()? {
            continue;
        } // legacy adapter owns legacy receipts
        let _lock = lock(&root.join("generation.lock"))?;
        let mut value = generation(&root)?;
        if value.phase == GenerationPhase::Pending {
            process_utils::command_authority::Gate::try_acquire(&root)?.close()?;
            value.phase = GenerationPhase::Revoked;
            save(&root.join("generation.json"), &value)?;
        }
        ensure!(
            matches!(
                value.phase,
                GenerationPhase::Quiescent | GenerationPhase::Revoked
            ),
            "generation {} cleanup is unconfirmed: {:?}",
            value.id,
            value.phase
        );
        process_utils::guardian::recover(&root)?;
        process_utils::command_context::require_quiescent(&root.join("commands"))?;
    }
    Ok(())
}
