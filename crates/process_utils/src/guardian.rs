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
    // Buffer JSON before touching the shared filesystem; otherwise each JSON
    // fragment is a separate write and delays startup/cleanup acknowledgements.
    let bytes = serde_json::to_vec(receipt)?;
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(&bytes)?;
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
    let bytes = serde_json::to_vec(&value)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&bytes)?;
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
struct RegisteredCommand {
    root: PathBuf,
    frame: Vec<u8>,
    managed: bool,
}

async fn register_guardian(
    command: tokio::process::Command,
    work_root: &Path,
    command_record: Option<&Path>,
    declared_root: Option<&Path>,
) -> Result<RegisteredCommand> {
    let managed = crate::command_authority::managed_scope(work_root, declared_root)
        .with_context(|| format!("read command authority at {}", work_root.display()))?;
    let admission = if managed {
        let gate = crate::command_authority::Gate::observe_open(work_root)
            .await
            .with_context(|| format!("acquire command authority at {}", work_root.display()))?;
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
    let mut frame = serde_json::to_vec(&spec)?;
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
        command_digest: Sha256::digest(&frame)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
    };
    save(&root, &receipt)
        .with_context(|| format!("register command guardian at {}", root.display()))?;
    // Registered before closure; a late guardian must consume under the same gate.
    drop(admission);
    frame.push(b'\n');
    Ok(RegisteredCommand {
        root,
        frame,
        managed,
    })
}

pub async fn spawn_guarded(
    command: tokio::process::Command,
    work_root: &Path,
    command_record: Option<&Path>,
    capture: bool,
) -> Result<OwnedChild> {
    let declared_root = crate::command_authority::current_root();
    let RegisteredCommand {
        root,
        frame,
        managed,
    } = register_guardian(command, work_root, command_record, declared_root.as_deref()).await?;
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
    if managed {
        // Pin the protocol selected at registration, including older callers
        // discovered through positive receipt evidence. A transient read in the
        // new process must not switch this command to legacy owner authorization.
        guardian.env(crate::command_authority::WORK_ROOT_ENV, work_root);
    }
    #[cfg(unix)]
    guardian.process_group(0);
    #[cfg(windows)]
    guardian.creation_flags(0x0000_0200); // CREATE_NEW_PROCESS_GROUP; no kill-on-drop Job.
    let mut child = guardian.spawn().context("spawn command guardian")?;
    let mut lease = child.stdin.take().context("guardian lease pipe missing")?;
    // If this future is cancelled, lease EOF still directs the guardian to
    // cleanup; Pending remains fenced until recover proves its exact receipt.
    lease
        .write_all(&frame)
        .await
        .context("send guardian command")?;
    // Startup has one existing 10s budget. The shorter steady-state visibility
    // window must not abort this handshake before the guardian can acknowledge.
    wait_for_start(&mut child, &root, Duration::from_secs(10)).await?;
    Ok(OwnedChild::Guarded {
        child,
        lease: Some(lease),
        root,
        receipt_unavailable_since: None,
    })
}

async fn wait_for_start(
    child: &mut tokio::process::Child,
    root: &Path,
    budget: Duration,
) -> Result<()> {
    crate::observe::observe("command startup acknowledgement", budget, || {
        // No spawn or other side effect is retried. Retain the original Child
        // and validate each complete receipt; malformed/foreign records fail.
        let result = (|| {
            let receipt = read(root)
                .with_context(|| format!("read command startup receipt at {}", root.display()))?;
            if receipt.diagnostic_pid.is_some() || receipt.root_status.is_some() {
                return Ok(Some(()));
            }
            ensure!(
                child.try_wait()?.is_none(),
                "command guardian exited before command startup"
            );
            Ok(None)
        })();
        std::future::ready(result)
    })
    .await
}

/// Private binary entry: hold the authorization lock before checking owner and
/// before spawning any business command. EOF on the exact parent pipe cancels.
pub async fn run(root: &Path) -> Result<i32> {
    let declared_root = crate::command_authority::current_root();
    run_with_input(
        root,
        tokio::io::BufReader::new(tokio::io::stdin()),
        declared_root.as_deref(),
    )
    .await
}

async fn run_with_input(
    root: &Path,
    input: impl tokio::io::AsyncBufRead + Unpin,
    declared_root: Option<&Path>,
) -> Result<i32> {
    // 启动首读纳入有界观察（收据可见性，2026-09-28）：父进程刚写收据即 spawn
    // 本 guardian，共享挂载上 lock/receipt 可能短暂不可见。每轮重新取锁（轮间
    // 释放）；phase 已消费/撤销是身份拒绝，不在观察内。
    let (_lock, mut receipt) = crate::observe::observe(
        "command guardian startup",
        Duration::from_secs(3),
        || async {
            let lock = lock(root)?;
            let receipt = read(root)?;
            Ok(Some((lock, receipt)))
        },
    )
    .await?;
    ensure!(
        receipt.phase == "Pending",
        "guardian authorization already consumed/revoked"
    );
    let result = execute_unconsumed(root, &mut receipt, input, declared_root).await;
    if result.is_err() && receipt.phase == "Pending" {
        // Holding the original authorization lock and not having entered the
        // pre-spawn Running boundary proves no business child was started.
        receipt.phase = "Revoked".into();
        save(root, &receipt)?;
        confirm_command(root, &receipt)?;
    }
    result
}

async fn execute_unconsumed(
    root: &Path,
    receipt: &mut Receipt,
    mut input: impl tokio::io::AsyncBufRead + Unpin,
    declared_root: Option<&Path>,
) -> Result<i32> {
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
    let managed = crate::command_authority::managed_scope(work, declared_root)?;
    if !managed {
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
    let mut lease_byte = [0u8; 1];
    let admission = if managed {
        tokio::select! {
            biased;
            ended = input.read(&mut lease_byte) => {
                ended.context("observe guardian parent lease before command admission")?;
                bail!("guardian parent lease ended before command admission");
            }
            gate = crate::command_authority::Gate::observe_open(work) => Some(gate?),
        }
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
    // 观测性：两段无限重试各补首败 + 每 ~50 次重试（约 50 秒）一条 warn 心跳；
    // 退出条件不变（成功才 break）。
    let mut save_retries: u32 = 0;
    loop {
        match save(root, receipt) {
            Ok(()) => break,
            Err(error) => {
                save_retries += 1;
                crate::warn_retry_pending(
                    "guardian quiescent save",
                    save_retries,
                    format_args!("{error:#}"),
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let mut confirm_retries: u32 = 0;
    loop {
        match confirm_command(root, receipt) {
            Ok(()) => break,
            Err(error) => {
                confirm_retries += 1;
                crate::warn_retry_pending(
                    "guardian command confirm",
                    confirm_retries,
                    format_args!("{error:#}"),
                );
            }
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
/// Outcome of a scope-checked guardian recovery (recovery v2 plan §7.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// Every guardian and command record reached a settled state; no live
    /// in-flight command remains in the captured scope.
    Settled,
    /// A guardian process is still alive and owns its command; cleanup must
    /// wait or report the bounded stage instead of forcing anything.
    Stopping { detail: String },
}

/// Recovery v2（plan §7.1）：按已灭进程范围收束死守护进程的 Running 命令
/// 记录。守护进程自身死亡后再无人能观察其命令结果——运行授权随范围关闭，
/// 业务结果显式保持未知（不伪造 exit=0）；守护进程仍存活时如实报告
/// Stopping，绝不按 PID 数字强杀。PID 缺失的存量回执保持保守失败。
pub fn recover_with_scope_check(work_root: &Path) -> Result<RecoveryOutcome> {
    let root = work_root.join("guardians");
    if !root.try_exists()? {
        return Ok(RecoveryOutcome::Settled);
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
            "Running" => {
                let Some(pid) = receipt.diagnostic_pid else {
                    bail!("guardian command outcome unknown; original authorization preserved");
                };
                if crate::process_exists(pid)? {
                    return Ok(RecoveryOutcome::Stopping {
                        detail: format!("owned command {pid} is still running"),
                    });
                }
                // The guardian died without observing its command's exit. The
                // whole captured scope was already proven ended by the caller
                // (generation worker exit or process-space replacement), so
                // the run authorization closes here; the command's business
                // outcome stays explicitly unknown.
                receipt.phase = "Quiescent".into();
                receipt.root_status = None;
                save(&path, &receipt)?;
                settle_unobserved_command(&receipt)?;
            }
            other => bail!("guardian command outcome unknown: {other}"),
        }
    }
    Ok(RecoveryOutcome::Settled)
}

/// Mark a dead guardian's command record settled without inventing an exit.
fn settle_unobserved_command(receipt: &Receipt) -> Result<()> {
    let Some(path) = &receipt.command_record else {
        return Ok(());
    };
    let parent = path.parent().context("command parent missing")?;
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
    value["termination"] = "GuardianDiedResultUnknown".into();
    value["diagnostic_pid"] = serde_json::Value::Null;
    let bytes = serde_json::to_vec(&value)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write as _;
    file.write_all(&bytes)?;
    file.flush()?;
    file.as_file().sync_all()?;
    crate::atomic_file::persist(file, path).context("publish unobserved command settlement")?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

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
            let bytes = serde_json::to_vec(&value)?;
            let mut temp = tempfile::NamedTempFile::new_in(&commands)?;
            temp.write_all(&bytes)?;
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

    fn managed_work(root: &Path) -> PathBuf {
        let instance = uuid::Uuid::new_v4().to_string();
        let work = root.join("work").join(&instance);
        crate::command_context::create_durable_directory(&work).unwrap();
        std::fs::write(
            work.join("generation.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "id": instance, "supervisor": "test-supervisor",
                "token": "test-token", "intent": "run", "phase": "Running",
                "worker_pid": std::process::id(), "exit_code": null, "error": null
            }))
            .unwrap(),
        )
        .unwrap();
        work
    }

    #[cfg(unix)]
    async fn assert_pending(future: std::pin::Pin<&mut impl Future>) {
        let mut future = future;
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_admission_gap_preserves_pending_and_honors_revocation() {
        for (outcome, generation_missing) in [
            ("open", false),
            ("open", true),
            ("closed", false),
            ("cancelled", true),
        ] {
            let scope = tempfile::tempdir().unwrap();
            let work = managed_work(scope.path());
            crate::command_authority::Gate::try_acquire(&work)
                .unwrap()
                .initialize()
                .unwrap();
            let authority_path = work.join("command-admission.json");
            let authority = std::fs::read(&authority_path).unwrap();
            std::fs::remove_file(&authority_path).unwrap();
            if generation_missing {
                std::fs::remove_file(work.join("generation.json")).unwrap();
            }

            let marker = scope.path().join("started");
            let mut command = tokio::process::Command::new("sh");
            command.args(["-c", "printf 'started\\n' >> \"$1\"", "fixture"]);
            command.arg(&marker);
            let mut registration = Box::pin(register_guardian(command, &work, None, Some(&work)));
            // One explicit poll must observe the missing receipt and yield.
            // This is the synchronization point, not a sleep hoping to hit it.
            assert_pending(registration.as_mut()).await;
            assert!(!work.join("guardians").exists());
            drop(crate::command_authority::Gate::try_acquire(&work).unwrap());
            std::fs::write(&authority_path, &authority).unwrap();
            let registered = registration.await.unwrap();
            assert!(registered.managed);
            assert_eq!(read(&registered.root).unwrap().phase, "Pending");

            // Reproduce the same gap in the independently consumed launch.
            std::fs::remove_file(&authority_path).unwrap();
            let (mut lease, input) = tokio::io::duplex(8192);
            lease.write_all(&registered.frame).await.unwrap();
            let mut lease = Some(lease);
            let mut execution = Box::pin(run_with_input(
                &registered.root,
                tokio::io::BufReader::new(input),
                Some(&work),
            ));
            assert_pending(execution.as_mut()).await;
            assert_eq!(read(&registered.root).unwrap().phase, "Pending");
            assert!(!marker.exists());
            // The generation gate is available while observation sleeps.
            let gate = crate::command_authority::Gate::try_acquire(&work).unwrap();
            match outcome {
                "closed" => gate.close().unwrap(),
                "cancelled" => drop(lease.take()),
                _ => std::fs::write(&authority_path, &authority).unwrap(),
            }
            drop(gate);
            let result = tokio::time::timeout(
                if outcome == "open" {
                    Duration::from_secs(5)
                } else {
                    Duration::from_secs(1)
                },
                execution,
            )
            .await
            .unwrap();
            drop(lease);
            if outcome != "open" {
                let error = format!("{:#}", result.unwrap_err());
                assert!(error.contains(if outcome == "closed" {
                    "no longer accepts work"
                } else {
                    "parent lease ended"
                }));
                assert_eq!(read(&registered.root).unwrap().phase, "Revoked");
                assert!(!marker.exists(), "revocation must prevent business spawn");
            } else {
                assert_eq!(result.unwrap(), 0);
                assert_eq!(std::fs::read_to_string(marker).unwrap(), "started\n");
                assert_eq!(read(&registered.root).unwrap().phase, "Quiescent");
            }
            confirmed(&registered.root).unwrap();
        }
    }

    #[tokio::test]
    async fn managed_registration_missing_admission_expires_without_registering() {
        let scope = tempfile::tempdir().unwrap();
        let work = managed_work(scope.path());
        let command = tokio::process::Command::new(std::env::current_exe().unwrap());
        let result = tokio::time::timeout(
            Duration::from_secs(4),
            register_guardian(command, &work, None, None),
        )
        .await
        .expect("one admission budget must bound the whole registration");
        let error = match result {
            Ok(_) => panic!("persistent receipt absence must reject registration"),
            Err(error) => error,
        };
        assert!(crate::observe::is_not_found(&error));
        assert!(format!("{error:#}").contains("not visible within"));
        assert!(!work.join("guardians").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_receipt_uses_handshake_budget_without_replacing_child() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&root).unwrap();
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let original_pid = child.id();
        let receipt = Receipt {
            version: 2,
            id: root.file_name().unwrap().to_str().unwrap().into(),
            instance_id: "test-instance".into(),
            phase: "Running".into(),
            command_record: None,
            command_digest: "0".repeat(64),
            root_status: None,
            diagnostic_pid: original_pid,
        };
        let mut waiting = Box::pin(wait_for_start(&mut child, &root, Duration::from_secs(3)));
        // Explicitly enter the missing-receipt observation before crossing the
        // old 250ms steady-state limit. It is still the same startup handshake.
        assert_pending(waiting.as_mut()).await;
        tokio::time::sleep(RECEIPT_VISIBILITY_BUDGET + Duration::from_millis(100)).await;
        assert_pending(waiting.as_mut()).await;
        save(&root, &receipt).unwrap();
        waiting.await.unwrap();
        assert_eq!(child.id(), original_pid);
        assert!(child.try_wait().unwrap().is_none());

        std::fs::remove_file(root.join("receipt.json")).unwrap();
        let error = wait_for_start(&mut child, &root, Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(crate::observe::is_not_found(&error));
        std::fs::write(root.join("receipt.json"), "corrupt").unwrap();
        let error = wait_for_start(&mut child, &root, Duration::from_secs(3))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<serde_json::Error>().is_some());
        assert_eq!(child.id(), original_pid);
        child.kill().await.unwrap();
    }

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

    /// recovery v2（plan §7.1 反例）：守护进程死亡后，其 Running 命令记录
    /// 由范围收束关闭运行授权——命令 Quiescent + termination 显式标记
    /// 结果未知，不伪造 exit；守护进程仍存活时如实 Stopping，不强杀。
    #[cfg(unix)]
    #[test]
    fn scope_checked_recovery_settles_dead_guardian_without_inventing_exit() {
        let scope = tempfile::tempdir().unwrap();
        let work = scope.path().join("work").join("inst-t3");
        let commands = work.join("commands");
        crate::command_context::create_durable_directory(&commands).unwrap();
        // 死守护：pid 已退出（复用短命子进程）。
        let mut dead = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = dead.id();
        dead.wait().unwrap();
        let (guardian_root, command_path) =
            fixture_running_guardian(&work, "cmd-t3", Some(dead_pid));
        let outcome = recover_with_scope_check(&work).unwrap();
        assert_eq!(outcome, RecoveryOutcome::Settled);
        let receipt: Receipt = read(&guardian_root).unwrap();
        assert_eq!(receipt.phase, "Quiescent");
        assert_eq!(receipt.root_status, None, "结果不得伪造");
        let command: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&command_path).unwrap()).unwrap();
        assert_eq!(command["phase"], "Quiescent");
        assert_eq!(command["termination"], "GuardianDiedResultUnknown");
        assert!(command.get("exit_code").is_none() || command["exit_code"].is_null());

        // 活守护：Stopping，不触碰记录。
        let mut alive = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let (guardian_root, command_path) =
            fixture_running_guardian(&work, "cmd-t3-live", Some(alive.id()));
        let before = std::fs::read(guardian_root.join("receipt.json")).unwrap();
        let outcome = recover_with_scope_check(&work).unwrap();
        assert!(
            matches!(outcome, RecoveryOutcome::Stopping { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read(guardian_root.join("receipt.json")).unwrap(),
            before,
            "活守护的记录不得被改写"
        );
        let _ = command_path;
        alive.kill().unwrap();
        alive.wait().unwrap();
    }

    /// 存量回执无 PID：保持保守失败（不猜、不强收）。
    #[test]
    fn scope_checked_recovery_keeps_legacy_pidless_receipt_conservative() {
        let scope = tempfile::tempdir().unwrap();
        let work = scope.path().join("work").join("inst-t3b");
        crate::command_context::create_durable_directory(&work.join("guardians")).unwrap();
        fixture_running_guardian(&work, "cmd-legacy", None);
        assert!(recover_with_scope_check(&work).is_err());
    }

    fn fixture_running_guardian(
        work: &Path,
        command_id: &str,
        pid: Option<u32>,
    ) -> (PathBuf, PathBuf) {
        let instance = work
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap()
            .to_owned();
        let id = uuid::Uuid::new_v4().to_string();
        let root = work.join("guardians").join(&id);
        crate::command_context::create_durable_directory(&root).unwrap();
        let command_path = work.join("commands").join(format!("{command_id}.json"));
        crate::command_context::create_durable_directory(command_path.parent().unwrap()).unwrap();
        std::fs::write(
            &command_path,
            serde_json::json!({"version":1,"phase":"Running","identity":{"task_id":command_id}})
                .to_string(),
        )
        .unwrap();
        save(
            &root,
            &Receipt {
                version: 1,
                root_status: None,
                diagnostic_pid: pid,
                id,
                instance_id: instance,
                phase: "Running".into(),
                command_record: Some(command_path.clone()),
                command_digest: "0".repeat(64),
            },
        )
        .unwrap();
        (root, command_path)
    }
}
