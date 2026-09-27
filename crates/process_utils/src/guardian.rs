//! Same-binary child guardian. A pipe lease, never a PID, authorizes lifetime.
use crate::managed_tree::{ManagedChild, StopOutcome, spawn_managed};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
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
    command_digest: String,
    #[serde(default)]
    root_status: Option<i64>,
    #[serde(default)]
    diagnostic_pid: Option<u32>,
}
fn save(root: &Path, receipt: &Receipt) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer(&mut file, receipt)?;
    file.flush()?;
    file.as_file().sync_all()?;
    crate::atomic_file::persist(file, &root.join("receipt.json"))
        .with_context(|| format!("publish command guardian receipt at {}", root.display()))?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    Ok(())
}
fn read(root: &Path) -> Result<Receipt> {
    let path = root.join("receipt.json");
    let value: Receipt = serde_json::from_slice(
        &std::fs::read(&path)
            .with_context(|| format!("read command receipt {}", path.display()))?,
    )?;
    ensure!(
        matches!(value.version, 1 | 2)
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

const RECEIPT_VISIBILITY_BUDGET: Duration = Duration::from_millis(250);

fn missing_receipt(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

/// Poll only the original registered receipt. Shared filesystem observations
/// may briefly miss a concurrently replaced file. Absence never proves exit:
/// retain the same Child and return Pending for a short, non-resetting window.
fn observe_receipt(
    root: &Path,
    unavailable_since: &mut Option<std::time::Instant>,
) -> Result<Option<Receipt>> {
    match read(root) {
        Ok(receipt) => {
            *unavailable_since = None;
            Ok(Some(receipt))
        }
        Err(error) if missing_receipt(&error) => {
            let since = unavailable_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() < RECEIPT_VISIBILITY_BUDGET {
                Ok(None)
            } else {
                Err(error).context("command receipt remains unavailable")
            }
        }
        Err(error) => Err(error),
    }
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
        receipt_unavailable_since: Option<std::time::Instant>,
    },
}
impl OwnedChild {
    pub fn id(&self) -> Option<u32> {
        match self {
            Self::Direct(c) => c.id(),
            // Expose the command's PID for logs/metrics, not its guardian's.
            // The OS handle retained in `child` remains the stop authority.
            Self::Guarded { root, .. } => read(root).ok().and_then(|r| r.diagnostic_pid),
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
            Self::Direct(c) => c.wait_root().await.map_err(std::io::Error::other),
            Self::Guarded { .. } => loop {
                if let Some(status) = self.try_wait_root()? {
                    return Ok(status);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            },
        }
    }
    pub fn try_wait_root(&mut self) -> std::io::Result<Option<ExitStatus>> {
        match self {
            Self::Direct(c) => c.try_wait_root(),
            Self::Guarded {
                child,
                root,
                receipt_unavailable_since,
                ..
            } => {
                let Some(receipt) = observe_receipt(root, receipt_unavailable_since)
                    .map_err(std::io::Error::other)?
                else {
                    return Ok(None);
                };
                if let Some(raw) = receipt.root_status {
                    return decode_status(raw).map(Some);
                }
                if child.try_wait()?.is_some() {
                    return Err(std::io::Error::other(
                        "command guardian exited without a command exit receipt",
                    ));
                }
                Ok(None)
            }
        }
    }
    pub async fn stop(&mut self, grace: Duration) -> StopOutcome {
        match self {
            Self::Direct(child) => child.stop(grace).await,
            Self::Guarded {
                child, lease, root, ..
            } => {
                // Closing our exact pipe asks the guardian to stop. Killing the
                // guardian would destroy the only authoritative tree handle.
                lease.take();
                let deadline = tokio::time::Instant::now() + grace + Duration::from_secs(6);
                match tokio::time::timeout_at(deadline, child.wait()).await {
                    Ok(Ok(status))
                        if confirm_visible(
                            root,
                            deadline.min(tokio::time::Instant::now() + RECEIPT_VISIBILITY_BUDGET),
                        )
                        .await
                        .is_ok() =>
                    {
                        StopOutcome::Graceful(status)
                    }
                    _ => StopOutcome::Unconfirmed,
                }
            }
        }
    }
}

async fn confirm_visible(root: &Path, deadline: tokio::time::Instant) -> Result<()> {
    loop {
        match confirmed(root) {
            Ok(()) => return Ok(()),
            Err(error) if missing_receipt(&error) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error),
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
    crate::atomic_file::persist(file, path).context("publish command cleanup receipt")?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn confirmed(root: &Path) -> Result<()> {
    let _lock = lock(root)?;
    let receipt = read(root)?;
    let instance = root
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|s| s.to_str());
    ensure!(
        instance == Some(receipt.instance_id.as_str()),
        "guardian instance mismatch"
    );
    ensure!(
        matches!(receipt.phase.as_str(), "Quiescent" | "Revoked"),
        "guardian cleanup unconfirmed"
    );
    Ok(())
}

/// Capture bounded diagnostics while continuously draining both output pipes.
/// The deadline includes startup and execution; cleanup retains ownership even
/// when the caller's wait expires.
pub async fn output_owned(
    mut command: tokio::process::Command,
    budget: Duration,
) -> Result<std::process::Output> {
    let deadline = tokio::time::Instant::now() + budget;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = tokio::time::timeout_at(deadline, spawn_owned(command, None)).await??;
    let stdout = child.take_stdout();
    let stderr = child.take_stderr();
    let out = tokio::spawn(capture(stdout));
    let err = tokio::spawn(capture(stderr));
    let result = tokio::time::timeout_at(deadline, child.wait_root()).await;
    if child.stop(Duration::ZERO).await == StopOutcome::Unconfirmed {
        crate::command_context::retain_cleanup(Some(child), None);
        out.abort();
        err.abort();
        anyhow::bail!("owned command cleanup remains unconfirmed");
    }
    let status = match result {
        Ok(result) => result?,
        Err(error) => {
            out.abort();
            err.abort();
            return Err(error).context("owned command deadline exceeded");
        }
    };
    Ok(std::process::Output {
        status,
        stdout: out.await??,
        stderr: err.await??,
    })
}
async fn capture<R: tokio::io::AsyncRead + Unpin>(reader: Option<R>) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    if let Some(mut reader) = reader {
        let mut buffer = [0u8; 8192];
        loop {
            let n = reader.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            let keep = n.min((1024 * 1024_usize).saturating_sub(output.len()));
            output.extend_from_slice(&buffer[..keep]);
        }
    }
    Ok(output)
}
pub async fn spawn_owned(
    command: tokio::process::Command,
    record: Option<&crate::command_context::CommandRecord>,
) -> Result<OwnedChild> {
    spawn_owned_with_output(command, record, true).await
}

pub async fn spawn_owned_with_output(
    command: tokio::process::Command,
    record: Option<&crate::command_context::CommandRecord>,
    capture: bool,
) -> Result<OwnedChild> {
    match crate::command_context::CommandContext::current().and_then(|c| c.journal_root) {
        Some(commands) => {
            spawn_guarded(
                command,
                commands.parent().context("work root missing")?,
                record.and_then(|r| r.path()),
                capture,
            )
            .await
        }
        None => match crate::command_authority::current_root() {
            Some(root) => {
                spawn_guarded(command, &root, record.and_then(|r| r.path()), capture).await
            }
            None => Ok(OwnedChild::Direct(spawn_managed(command)?)),
        },
    }
}
pub async fn spawn_guarded(
    command: tokio::process::Command,
    work_root: &Path,
    command_record: Option<&Path>,
    capture: bool,
) -> Result<OwnedChild> {
    let admission = if crate::command_authority::is_managed(work_root)
        .with_context(|| format!("read command authority at {}", work_root.display()))?
    {
        let gate = crate::command_authority::Gate::acquire(work_root)
            .await
            .with_context(|| format!("acquire command authority at {}", work_root.display()))?;
        gate.require_open()
            .context("check command admission before guardian registration")?;
        Some(gate)
    } else {
        None
    };
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
    crate::command_context::create_durable_directory(&root)
        .with_context(|| format!("create command guardian directory {}", root.display()))?;
    let receipt = Receipt {
        version: 2,
        root_status: None,
        diagnostic_pid: None,
        id: root
            .file_name()
            .context("guardian id missing")?
            .to_string_lossy()
            .into(),
        instance_id,
        phase: "Pending".into(),
        command_record: command_record.map(Path::to_path_buf),
        command_digest: Sha256::digest(serde_json::to_vec(&spec)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    };
    save(&root, &receipt)
        .with_context(|| format!("register command guardian at {}", root.display()))?;
    // Registered before closure; a late guardian must consume under the same gate.
    drop(admission);
    let mut guardian = tokio::process::Command::new(
        std::env::current_exe().context("resolve command guardian executable")?,
    );
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
    // Spawn means the actual command has started, not merely its guardian.
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut unavailable_since = None;
        loop {
            let receipt = observe_receipt(&root, &mut unavailable_since)
                .with_context(|| format!("read command startup receipt at {}", root.display()))?;
            let Some(receipt) = receipt else {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            };
            if receipt.diagnostic_pid.is_some() || receipt.root_status.is_some() {
                return Ok::<_, anyhow::Error>(());
            }
            ensure!(
                child.try_wait()?.is_none(),
                "command guardian exited before command startup"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("command startup acknowledgement timed out")??;
    Ok(OwnedChild::Guarded {
        child,
        lease: Some(lease),
        root,
        receipt_unavailable_since: None,
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
    let result = execute_unconsumed(root, &mut receipt).await;
    if result.is_err() && receipt.phase == "Pending" {
        // Holding the original authorization lock and not having entered the
        // pre-spawn Running boundary proves no business child was started.
        receipt.phase = "Revoked".into();
        save(root, &receipt)?;
        confirm_command(root, &receipt)?;
    }
    result
}

async fn execute_unconsumed(root: &Path, receipt: &mut Receipt) -> Result<i32> {
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
    let work = root
        .parent()
        .and_then(Path::parent)
        .context("guardian work root missing")?;
    // Older proxy receipts keep their existing authority contract. New managed
    // generations use the shared gate below, never proxy-specific owner.json.
    if !crate::command_authority::is_managed(work)? {
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
    }
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut bytes = Vec::new();
    (&mut input)
        .take(1024 * 1024 + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= 1024 * 1024 && bytes.last() == Some(&b'\n'),
        "invalid guardian command frame"
    );
    let spec: Spec = serde_json::from_slice(&bytes)?;
    ensure!(
        Sha256::digest(serde_json::to_vec(&spec)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            == receipt.command_digest,
        "guardian command differs from original authorized specification"
    );
    let work = root
        .parent()
        .and_then(Path::parent)
        .context("guardian work root missing")?;
    let admission = if crate::command_authority::is_managed(work)? {
        let gate = crate::command_authority::Gate::acquire(work).await?;
        gate.require_open()?;
        Some(gate)
    } else {
        None
    };
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
    crate::command_authority::detach_command(&mut command);
    // Mark the spawn window before the side effect. A guardian crash during
    // spawn must not be misread as unconsumed Pending during recovery.
    receipt.phase = "Running".into();
    save(root, receipt)?;
    let mut child = match spawn_managed(command) {
        Ok(child) => child,
        Err(error) => {
            receipt.phase = "Quiescent".into();
            save(root, receipt)?;
            return Err(error);
        }
    };
    receipt.diagnostic_pid = child.id();
    // A persistence failure must still retain and stop the actual command.
    if let Err(error) = save(root, receipt) {
        eprintln!("record owned root start: {error:#}");
    }
    drop(admission);
    let mut lease_byte = [0u8; 1];
    let exit = tokio::select! { result = child.wait_root() => result.ok(), _ = input.read(&mut lease_byte) => None };
    receipt.root_status = exit.map(encode_status);
    if let Err(error) = save(root, receipt) {
        eprintln!("record owned root exit: {error:#}");
    }
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
        if save(root, receipt).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    loop {
        if confirm_command(root, receipt).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(exit.and_then(|s| s.code()).unwrap_or(1))
}

#[cfg(unix)]
fn encode_status(status: ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    i64::from(status.into_raw())
}
#[cfg(windows)]
fn encode_status(status: ExitStatus) -> i64 {
    // Windows always has an exit code; preserve all DWORD bits (including
    // exception codes represented as a negative i32 by std).
    i64::from(status.code().unwrap_or(1) as u32)
}
#[cfg(unix)]
fn decode_status(raw: i64) -> std::io::Result<ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    Ok(ExitStatus::from_raw(
        i32::try_from(raw).map_err(std::io::Error::other)?,
    ))
}
#[cfg(windows)]
fn decode_status(raw: i64) -> std::io::Result<ExitStatus> {
    use std::os::windows::process::ExitStatusExt;
    Ok(ExitStatus::from_raw(
        u32::try_from(raw).map_err(std::io::Error::other)?,
    ))
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

/// Platform-confirmed termination of the entire captured physical domain.
/// Caller must hold the original scope/generation lock and close admission.
/// Never use this for a refused port, missing PID or changed namespace alone.
pub fn confirm_physical_domain_exit(work_root: &Path) -> Result<()> {
    let dir = work_root.join("guardians");
    if dir.try_exists()? {
        for entry in std::fs::read_dir(dir)? {
            let root = entry?.path();
            let _lock = lock(&root)?;
            let mut receipt = read(&root)?;
            ensure!(
                work_root.file_name().and_then(|s| s.to_str()) == Some(&receipt.instance_id),
                "physical exit guardian identity mismatch"
            );
            receipt.phase = "Quiescent".into();
            // root_status remains unknown if it was not observed. Cleanup is
            // not evidence that a command or migration returned success.
            save(&root, &receipt)?;
            confirm_command(&root, &receipt)?;
        }
    }
    let commands = work_root.join("commands");
    if commands.try_exists()? {
        for entry in std::fs::read_dir(&commands)? {
            let path = entry?.path();
            let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            ensure!(
                value["version"] == 1
                    && matches!(
                        value["phase"].as_str(),
                        Some("SpawnPending" | "Running" | "Quiescent")
                    ),
                "invalid command cleanup record"
            );
            value["phase"] = "Quiescent".into();
            value["termination"] = "PhysicalDomainExited".into();
            let mut temp = tempfile::NamedTempFile::new_in(&commands)?;
            serde_json::to_writer(&mut temp, &value)?;
            temp.flush()?;
            temp.as_file().sync_all()?;
            crate::atomic_file::persist(temp, &path)?;
        }
        #[cfg(unix)]
        File::open(commands)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[tokio::test]
    async fn receipt_visibility_gap_retains_child_but_missing_or_corrupt_receipt_is_not_success() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&root).unwrap();
        let receipt = Receipt {
            version: 2,
            id: root.file_name().unwrap().to_str().unwrap().into(),
            instance_id: "test-instance".into(),
            phase: "Running".into(),
            command_record: None,
            command_digest: "0".repeat(64),
            root_status: None,
            diagnostic_pid: None,
        };
        save(&root, &receipt).unwrap();
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let original_pid = child.id();
        let mut owned = OwnedChild::Guarded {
            child,
            lease: None,
            root: root.clone(),
            receipt_unavailable_since: None,
        };
        assert!(owned.try_wait_root().unwrap().is_none());
        let path = root.join("receipt.json");
        let retained = root.join("retained.json");
        std::fs::rename(&path, &retained).unwrap();
        assert!(owned.try_wait_root().unwrap().is_none());
        std::fs::rename(&retained, &path).unwrap();
        assert!(owned.try_wait_root().unwrap().is_none());
        std::fs::write(&path, "corrupt").unwrap();
        assert!(owned.try_wait_root().is_err());
        save(&root, &receipt).unwrap();
        assert!(owned.try_wait_root().unwrap().is_none());
        std::fs::rename(path, retained).unwrap();
        assert!(owned.try_wait_root().unwrap().is_none());
        if let OwnedChild::Guarded {
            receipt_unavailable_since,
            ..
        } = &mut owned
        {
            *receipt_unavailable_since = Some(std::time::Instant::now() - Duration::from_secs(1));
        }
        assert!(
            owned.try_wait_root().is_err(),
            "permanent absence must fail"
        );
        if let OwnedChild::Guarded { child, .. } = &mut owned {
            assert_eq!(
                child.id(),
                original_pid,
                "never launch a replacement command"
            );
            child.kill().await.unwrap();
        }
    }

    #[tokio::test]
    async fn stopping_owner_revokes_unconsumed_guardian_without_spawning() {
        let scope = tempfile::tempdir().unwrap();
        let instance = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let root = scope
            .path()
            .join("work")
            .join(&instance)
            .join("guardians")
            .join(&id);
        crate::command_context::create_durable_directory(&root).unwrap();
        std::fs::write(
            scope.path().join("owner.json"),
            serde_json::to_vec(&serde_json::json!({"instance_id":instance,"phase":"Stopping"}))
                .unwrap(),
        )
        .unwrap();
        let receipt = Receipt {
            root_status: None,
            diagnostic_pid: None,
            version: 1,
            id,
            instance_id: instance,
            phase: "Pending".into(),
            command_record: None,
            command_digest: "0".repeat(64),
        };
        save(&root, &receipt).unwrap();
        // Rejection occurs before reading a spec or spawning a child.
        assert!(run(&root).await.is_err());
        assert_eq!(read(&root).unwrap().phase, "Revoked");
        confirmed(&root).unwrap();
        assert!(run(&root).await.is_err());
        assert_eq!(read(&root).unwrap().phase, "Revoked");
    }
    #[tokio::test]
    async fn consumed_unknown_guardian_is_never_revoked_by_rejection() {
        let scope = tempfile::tempdir().unwrap();
        let instance = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let root = scope
            .path()
            .join("work")
            .join(&instance)
            .join("guardians")
            .join(&id);
        crate::command_context::create_durable_directory(&root).unwrap();
        save(
            &root,
            &Receipt {
                version: 1,
                root_status: None,
                diagnostic_pid: None,
                id,
                instance_id: instance,
                phase: "Running".into(),
                command_record: None,
                command_digest: "0".repeat(64),
            },
        )
        .unwrap();
        let before = std::fs::read(root.join("receipt.json")).unwrap();
        assert!(run(&root).await.is_err());
        assert!(confirmed(&root).is_err());
        assert_eq!(std::fs::read(root.join("receipt.json")).unwrap(), before);
    }
}
