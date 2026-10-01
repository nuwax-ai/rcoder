//! Generation-scoped command admission shared by CLI supervisors and guardians.
//! The gate covers both registration and consumption; closing it never means
//! previously consumed commands have stopped.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

pub const WORK_ROOT_ENV: &str = "RCODER_COMMAND_WORK_ROOT";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    version: u32,
    generation: String,
    accepting: bool,
}

pub struct Gate {
    _file: File,
    root: PathBuf,
}

impl Gate {
    pub fn try_acquire(root: &Path) -> Result<Self> {
        crate::command_context::create_durable_directory(root)?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("command-admission.lock"))?;
        // Never park the control runtime behind a suspended command guardian.
        file.try_lock().context("command admission is busy")?;
        Ok(Self {
            _file: file,
            root: root.into(),
        })
    }

    pub async fn acquire(root: &Path) -> Result<Self> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match Self::try_acquire(root) {
                Ok(gate) => return Ok(gate),
                Err(error)
                    if error
                        .downcast_ref::<std::fs::TryLockError>()
                        .is_some_and(|e| matches!(e, std::fs::TryLockError::WouldBlock))
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Registration and consumption both require the same live generation gate.
    /// Each failed observation drops its lock before waiting; a closed or invalid
    /// authority is a rejection, while a briefly missing receipt remains pending.
    pub(crate) async fn observe_open(root: &Path) -> Result<Self> {
        crate::observe::observe(
            "command admission",
            std::time::Duration::from_secs(3),
            || async {
                let gate = Self::try_acquire(root)?;
                gate.require_open()?;
                Ok(Some(gate))
            },
        )
        .await
    }

    pub fn initialize(&self) -> Result<()> {
        ensure!(
            !self.root.join("command-admission.json").try_exists()?,
            "command generation already initialized"
        );
        self.write(true)
    }

    pub fn close(&self) -> Result<()> {
        if self.root.join("command-admission.json").try_exists()? {
            let old: Authority =
                serde_json::from_slice(&std::fs::read(self.root.join("command-admission.json"))?)?;
            ensure!(
                old.version == 1 && old.generation == self.generation()?,
                "command generation identity changed"
            );
            if !old.accepting {
                return Ok(());
            }
        }
        self.write(false)
    }

    pub fn require_open(&self) -> Result<()> {
        let value: Authority =
            serde_json::from_slice(&std::fs::read(self.root.join("command-admission.json"))?)?;
        ensure!(
            value.version == 1 && value.generation == self.generation()? && value.accepting,
            "command generation no longer accepts work"
        );
        Ok(())
    }

    fn generation(&self) -> Result<&str> {
        self.root
            .file_name()
            .and_then(|s| s.to_str())
            .context("invalid command generation root")
    }

    fn write(&self, accepting: bool) -> Result<()> {
        let bytes = serde_json::to_vec(&Authority {
            version: 1,
            generation: self.generation()?.into(),
            accepting,
        })?;
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.as_file().sync_all()?;
        crate::atomic_file::persist(file, &self.root.join("command-admission.json"))
            .context("publish command admission")?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
}

/// Explicit in-process session scope（recovery v2 R1）：统一 owner 的业务
/// 会话在 launch 时安装当前工作范围。区别于 env（旧 worker 链）与
/// task-local（不穿透嵌套 spawn），进程级显式会话上下文覆盖本 owner
/// 进程内全部嵌套 tokio::spawn 的受管命令；单 owner 单工作区进程模型下
/// 无歧义。业务结束/换代时由会话改写。
static SESSION_WORK_ROOT: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

pub fn set_session_work_root(root: Option<PathBuf>) {
    let mut slot = match SESSION_WORK_ROOT.write() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    // 锁中毒只意味着某次写入途中 panic；本槽是可整体替换的枚举值，
    // 覆盖写即恢复一致，不需要未知状态传播。
    *slot = root;
}

pub fn session_work_root() -> Option<PathBuf> {
    match SESSION_WORK_ROOT.read() {
        Ok(slot) => slot.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

pub fn current_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(WORK_ROOT_ENV)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
    {
        return Some(root);
    }
    session_work_root()
}

pub fn is_managed(root: &Path) -> Result<bool> {
    managed_scope(root, current_root().as_deref())
}

pub(crate) fn managed_scope(root: &Path, declared_root: Option<&Path>) -> Result<bool> {
    if let Some(declared) = declared_root {
        // The supervisor already passes this generation root to its worker.
        // Keep that protocol selection even when both receipts are temporarily
        // invisible. This selects the gate; it never replaces gate authorization.
        ensure!(
            declared == root
                || std::fs::canonicalize(declared)
                    .context("resolve declared command generation")?
                    == std::fs::canonicalize(root).context("resolve command generation")?,
            "command work root differs from the supervised generation"
        );
        return Ok(true);
    }
    // Older/manual entry points have no inherited declaration. Positive evidence
    // of either managed record still requires the gate, even if its peer is missing.
    // The caller retains this choice for the entire launch and validates the
    // existing owner receipt/lock before executing any legacy command.
    Ok(root.join("command-admission.json").try_exists()?
        || root.join("generation.json").try_exists()?)
}

/// An independently owned CLI must not inherit its launcher's worker identity.
pub fn detach_command(command: &mut tokio::process::Command) {
    for name in [
        WORK_ROOT_ENV,
        "RCODER_SUPERVISOR_WORKER",
        "RCODER_SUPERVISOR_TOKEN",
        "FILE_SERVER_PROXY_OWNER_SUPERVISOR",
        "FILE_SERVER_PROXY_LAUNCH_ID",
    ] {
        command.env_remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_generation_never_falls_back_when_both_receipts_are_missing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("generation");
        let other = directory.path().join("other");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&other).unwrap();
        assert!(managed_scope(&root, Some(&root)).unwrap());
        assert!(managed_scope(&root, Some(&other)).is_err());
        assert!(!managed_scope(&root, None).unwrap());
        std::fs::write(root.join("generation.json"), "{}").unwrap();
        assert!(managed_scope(&root, None).unwrap());
    }
}
