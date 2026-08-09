//! 子进程有界输出捕获。

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::error::{AppError, AppResult};

const MAX_CAPTURE_BYTES: usize = 50 * 1024 * 1024;

pub(super) struct CaptureResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

pub(super) async fn capture_command(
    command: &mut Command,
    label: &str,
    timeout_secs: u64,
) -> AppResult<CaptureResult> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    command.kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| AppError::system(format!("spawn {label} failed: {error}")))?;
    let child_pid = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::system(format!("{label} stdout pipe is unavailable")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::system(format!("{label} stderr pipe is unavailable")))?;
    let stdout_task = tokio::spawn(read_bounded(stdout));
    let stderr_task = tokio::spawn(read_bounded(stderr));

    let status = match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(result) => Some(
            result.map_err(|error| AppError::system(format!("{label} wait failed: {error}")))?,
        ),
        Err(_) => {
            if let Some(pid) = child_pid {
                crate::service::dev_server::process::kill_process_group_force(pid);
            }
            if let Err(e) = child.kill().await {
                tracing::warn!(error = %e, "kill child on capture timeout failed (skipping)");
            }
            if let Err(e) = child.wait().await {
                tracing::warn!(error = %e, "wait to reap child on capture timeout failed (skipping)");
            }
            None
        }
    };

    let stdout = join_reader(stdout_task, label, "stdout").await?;
    let mut stderr = join_reader(stderr_task, label, "stderr").await?;
    let exit_code = status
        .as_ref()
        .and_then(std::process::ExitStatus::code)
        .unwrap_or(-1);
    if status.is_none() {
        if !stderr.is_empty() && !stderr.ends_with(b"\n") {
            stderr.push(b'\n');
        }
        stderr.extend_from_slice(format!("timed out after {timeout_secs}s").as_bytes());
    }
    Ok(CaptureResult {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
    })
}

pub(super) async fn run_capture(
    program: &str,
    args: &[&str],
    cwd: &std::path::Path,
    timeout_secs: u64,
) -> AppResult<(String, String, i32)> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd);
    if let Ok(path) = std::env::var("PATH") {
        command.env("PATH", path);
    }
    if let Ok(home) = std::env::var("HOME") {
        command.env("HOME", home);
    }
    command
        .env_remove("CI")
        .env_remove("NPM_CONFIG_PRODUCTION")
        .env("NODE_ENV", "development");
    let result = capture_command(&mut command, program, timeout_secs).await?;
    Ok((result.stdout, result.stderr, result.exit_code))
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut stored = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(stored);
        }
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(stored.len());
        if remaining > 0 {
            stored.extend_from_slice(&buffer[..read.min(remaining)]);
        }
        // 超限后继续 drain pipe，防止子进程因管道写满而死锁。
    }
}

async fn join_reader(
    task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    label: &str,
    stream: &str,
) -> AppResult<Vec<u8>> {
    task.await
        .map_err(|error| AppError::system(format!("{label} {stream} task failed: {error}")))?
        .map_err(|error| AppError::system(format!("read {label} {stream}: {error}")))
}
