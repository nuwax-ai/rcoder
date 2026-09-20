//! Same-binary child guardian. A pipe lease, never a PID, authorizes lifetime.
use crate::managed_tree::{ManagedChild, StopOutcome, spawn_managed};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, Option<OsString>)>,
    cwd: Option<PathBuf>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    id: String,
    instance_id: String,
    phase: String,
    command_record: Option<PathBuf>,
}
fn save(root: &Path, receipt: &Receipt) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer(&mut file, receipt)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(root.join("receipt.json"))?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    Ok(())
}
fn read(root: &Path) -> Result<Receipt> {
    let value: Receipt = serde_json::from_slice(&std::fs::read(root.join("receipt.json"))?)?;
    ensure!(
        value.version == 1
            && matches!(
                value.phase.as_str(),
                "Pending" | "Running" | "Quiescent" | "Revoked"
            ),
        "unknown guardian receipt"
    );
    ensure!(
        root.file_name().and_then(|s| s.to_str()) == Some(&value.id),
        "guardian identity mismatch"
    );
    Ok(value)
}
fn lock(root: &Path) -> Result<File> {
    let lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("guardian.lock"))?;
    lock.try_lock()
        .context("guardian still owns its command tree")?;
    Ok(lock)
}

pub enum OwnedChild {
    Direct(ManagedChild),
    Guarded {
        child: tokio::process::Child,
        lease: Option<tokio::process::ChildStdin>,
        root: PathBuf,
    },
}
impl OwnedChild {
    pub fn id(&self) -> Option<u32> {
        match self {
            Self::Direct(c) => c.id(),
            Self::Guarded { child, .. } => child.id(),
        }
    }
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        match self {
            Self::Direct(c) => c.take_stdout(),
            Self::Guarded { child, .. } => child.stdout.take(),
        }
    }
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        match self {
            Self::Direct(c) => c.take_stderr(),
            Self::Guarded { child, .. } => child.stderr.take(),
        }
    }
    pub async fn wait_root(&mut self) -> std::io::Result<ExitStatus> {
        match self {
            Self::Direct(c) => c.wait_root().await,
            Self::Guarded { child, .. } => child.wait().await,
        }
    }
    pub fn try_wait_root(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self {
            Self::Direct(c) => c.try_wait_root(),
            Self::Guarded { child, .. } => child.try_wait(),
        }
    }
    pub async fn stop(&mut self, grace: Duration) -> StopOutcome {
        match self {
            Self::Direct(child) => child.stop(grace).await,
            Self::Guarded { child, lease, root } => {
                // Closing our exact pipe asks the guardian to stop. Killing the
                // guardian would destroy the only authoritative tree handle.
                lease.take();
                match tokio::time::timeout(grace + Duration::from_secs(6), child.wait()).await {
                    Ok(Ok(status)) if confirmed(root).is_ok() => StopOutcome::Graceful(status),
                    _ => StopOutcome::Unconfirmed,
                }
            }
        }
    }
}
fn confirm_command(root: &Path, receipt: &Receipt) -> Result<()> {
    let Some(path) = &receipt.command_record else {
        return Ok(());
    };
    let work = root
        .parent()
        .and_then(Path::parent)
        .context("guardian work root missing")?;
    ensure!(
        path.parent() == Some(work.join("commands").as_path()),
        "guardian command record outside original instance"
    );
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        value["version"] == 1
            && matches!(
                value["phase"].as_str(),
                Some("SpawnPending" | "Running" | "Quiescent")
            ),
        "unknown command record"
    );
    value["phase"] = "Quiescent".into();
    value["diagnostic_pid"] = serde_json::Value::Null;
    let parent = path.parent().context("command parent missing")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, &value)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn confirmed(root: &Path) -> Result<()> {
    let _lock = lock(root)?;
    ensure!(
        read(root)?.phase == "Quiescent",
        "guardian cleanup unconfirmed"
    );
    Ok(())
}
pub async fn spawn_owned(
    command: tokio::process::Command,
    record: Option<&crate::command_context::CommandRecord>,
) -> Result<OwnedChild> {
    match crate::command_context::CommandContext::current().and_then(|c| c.journal_root) {
        Some(commands) => {
            spawn_guarded(
                command,
                commands.parent().context("work root missing")?,
                record.and_then(|r| r.path()),
                true,
            )
            .await
        }
        None => Ok(OwnedChild::Direct(spawn_managed(command)?)),
    }
}
pub async fn spawn_guarded(
    command: tokio::process::Command,
    work_root: &Path,
    command_record: Option<&Path>,
    capture: bool,
) -> Result<OwnedChild> {
    let std = command.as_std();
    let spec = Spec {
        program: std.get_program().into(),
        args: std.get_args().map(Into::into).collect(),
        env: std
            .get_envs()
            .map(|(k, v)| (k.into(), v.map(Into::into)))
            .collect(),
        cwd: std.get_current_dir().map(Into::into),
    };
    let instance_id = work_root
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid owner work root")?
        .to_owned();
    let root = work_root
        .join("guardians")
        .join(uuid::Uuid::new_v4().to_string());
    crate::command_context::create_durable_directory(&root)?;
    let receipt = Receipt {
        version: 1,
        id: root
            .file_name()
            .context("guardian id missing")?
            .to_string_lossy()
            .into(),
        instance_id,
        phase: "Pending".into(),
        command_record: command_record.map(Path::to_path_buf),
    };
    save(&root, &receipt)?;
    let mut guardian = tokio::process::Command::new(std::env::current_exe()?);
    guardian
        .arg("--native-command-guardian")
        .arg(&root)
        .stdin(Stdio::piped())
        .stdout(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stderr(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        });
    #[cfg(unix)]
    guardian.process_group(0);
    #[cfg(windows)]
    guardian.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP; no kill-on-drop Job.
    let mut child = guardian.spawn().context("spawn command guardian")?;
    let mut lease = child.stdin.take().context("guardian lease pipe missing")?;
    let mut bytes = serde_json::to_vec(&spec)?;
    bytes.push(b'\n');
    // If this future is cancelled, lease EOF still directs the guardian to
    // cleanup; Pending remains fenced until recover proves its exact receipt.
    lease
        .write_all(&bytes)
        .await
        .context("send guardian command")?;
    Ok(OwnedChild::Guarded {
        child,
        lease: Some(lease),
        root,
    })
}

/// Private binary entry: hold the authorization lock before checking owner and
/// before spawning any business command. EOF on the exact parent pipe cancels.
pub async fn run(root: &Path) -> Result<i32> {
    let _lock = lock(root)?;
    let mut receipt = read(root)?;
    ensure!(
        receipt.phase == "Pending",
        "guardian authorization already consumed/revoked"
    );
    if let Some(path) = &receipt.command_record {
        let work = root
            .parent()
            .and_then(Path::parent)
            .context("guardian work root missing")?;
        ensure!(
            path.parent() == Some(work.join("commands").as_path()),
            "guardian command record outside original instance"
        );
    }
    let scope = root
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context("invalid guardian scope")?;
    let owner: serde_json::Value =
        serde_json::from_slice(&std::fs::read(scope.join("owner.json"))?)?;
    ensure!(
        owner["instance_id"] == receipt.instance_id
            && matches!(owner["phase"].as_str(), Some("Starting" | "Running")),
        "original owner no longer authorizes spawn"
    );
    let owner_lock = File::options()
        .read(true)
        .write(true)
        .open(scope.join("owner.lock"))?;
    match owner_lock.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => {}
        Ok(()) => bail!("original owner has exited"),
        Err(error) => bail!("cannot verify original owner lock: {error}"),
    }
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut bytes = Vec::new();
    input.read_until(b'\n', &mut bytes).await?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= 1024 * 1024,
        "invalid guardian command frame"
    );
    let spec: Spec = serde_json::from_slice(&bytes)?;
    let mut command = tokio::process::Command::new(spec.program);
    command
        .args(spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (key, value) in spec.env {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    if let Some(cwd) = spec.cwd {
        command.current_dir(cwd);
    }
    // Mark the spawn window before the side effect. A guardian crash during
    // spawn must not be misread as unconsumed Pending during recovery.
    receipt.phase = "Running".into();
    save(root, &receipt)?;
    let mut child = match spawn_managed(command) {
        Ok(child) => child,
        Err(error) => {
            receipt.phase = "Quiescent".into();
            save(root, &receipt)?;
            return Err(error);
        }
    };
    let mut lease_byte = [0u8; 1];
    let exit = tokio::select! { result = child.wait_root() => result.ok(), _ = input.read(&mut lease_byte) => None };
    loop {
        if !matches!(
            child.stop(Duration::from_secs(1)).await,
            StopOutcome::Unconfirmed
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    receipt.phase = "Quiescent".into();
    loop {
        if save(root, &receipt).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    loop {
        if confirm_command(root, &receipt).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(exit.and_then(|s| s.code()).unwrap_or(1))
}

/// Called only while holding the original scope owner lock. Revocation under
/// the same guardian lock prevents a late Pending guardian from spawning.
pub fn recover(work_root: &Path) -> Result<()> {
    let root = work_root.join("guardians");
    if !root.try_exists()? {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let _lock = lock(&path)?;
        let mut receipt = read(&path)?;
        ensure!(
            work_root.file_name().and_then(|s| s.to_str()) == Some(&receipt.instance_id),
            "guardian belongs to different instance"
        );
        match receipt.phase.as_str() {
            "Pending" => {
                receipt.phase = "Revoked".into();
                save(&path, &receipt)?;
                confirm_command(&path, &receipt)?;
            }
            "Quiescent" | "Revoked" => {
                confirm_command(&path, &receipt)?;
            }
            _ => bail!("guardian command outcome unknown; original authorization preserved"),
        }
    }
    Ok(())
}
