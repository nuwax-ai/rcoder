//! 稳定 pnpm CLI 后端：进程管理、超时和日志管道。

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use chrono::Local;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use super::classify::classify_failure;
use super::error::InstallError;
use super::protocol::{ObservedLine, observe_event};
use super::types::{InstallOptions, InstallOutcome, InstallSummary, LogFiles};
use crate::service::dev_server::{log, process};

const CAPTURE_LIMIT: usize = 1024 * 1024;

pub(super) async fn install(
    cwd: &Path,
    options: &InstallOptions,
    logs: Option<&LogFiles>,
    timeout_secs: u64,
) -> Result<InstallOutcome, InstallError> {
    let mut args = vec!["--reporter=ndjson".to_string(), "install".to_string()];
    if options.prefer_offline {
        args.push("--prefer-offline".to_string());
    }
    // file-server 非 TTY (stdin null) spawn pnpm: node_modules 与 lockfile 不一致时, pnpm 要
    // 交互确认 purge modules 目录, 无 TTY 则 abort (ERR_PNPM_ABORTED_REMOVE_MODULES_DIR_NO_TTY)
    // → install 永久失败、vite 起不来。confirmModulesPurge=false 跳过确认 (对齐 pnpm 源码
    // deps-installer/.../validateModules.ts:152 + PnpmError hint 推荐; CLI 用 camelCase,
    // .npmrc 才用 kebab confirm-modules-purge)。兜底所有 install 路径 (dev_server/exec/build)。
    // 另一解法是 CI=true (源码 index.ts: confirmModulesPurge && !ci), 但 file-server
    // env_remove(CI), 且 CI 模式会影响 reporter, 故走精确配置。
    // 不用 --config.dangerously-allow-all-builds: pnpm 10.x 下它与内置 neverBuiltDependencies 冲突。
    args.push("--config.confirmModulesPurge=false".to_string());
    args.extend(options.extra_args.iter().cloned());

    let mut command = Command::new("pnpm");
    command.args(&args).current_dir(cwd);
    if let Ok(path) = std::env::var("PATH") {
        command.env("PATH", path);
    }
    if let Ok(home) = std::env::var("HOME") {
        command.env("HOME", home);
    }
    command.env_remove("CI");
    command.env_remove("NPM_CONFIG_PRODUCTION");
    command.env("NODE_ENV", "development");
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);

    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|source| InstallError::Spawn { source })?;
    let child_pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_logs = logs.cloned();
    let stderr_logs = logs.cloned();

    let stdout_task = tokio::spawn(async move {
        match stdout {
            Some(stream) => read_stream(stream, stdout_logs.as_ref(), false).await,
            None => StreamResult::default(),
        }
    });
    let stderr_task = tokio::spawn(async move {
        match stderr {
            Some(stream) => read_stream(stream, stderr_logs.as_ref(), true).await,
            None => StreamResult::default(),
        }
    });

    let status = match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(source)) => {
            terminate_and_reap(&mut child, child_pid).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(InstallError::Wait { source });
        }
        Err(_) => {
            terminate_and_reap(&mut child, child_pid).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(InstallError::TimedOut { timeout_secs });
        }
    };

    let stdout_result = stdout_task.await.map_err(|error| InstallError::Wait {
        source: std::io::Error::other(format!("pnpm stdout reader task failed: {error}")),
    })?;
    let stderr_result = stderr_task.await.map_err(|error| InstallError::Wait {
        source: std::io::Error::other(format!("pnpm stderr reader task failed: {error}")),
    })?;
    let stream_errors: Vec<String> = [stdout_result.error.clone(), stderr_result.error.clone()]
        .into_iter()
        .flatten()
        .collect();
    for error in &stream_errors {
        tracing::warn!(%error, "pnpm output stream was not captured completely");
    }
    let mut summary = stderr_result.summary;
    summary.merge(stdout_result.summary);
    if status.success() {
        tracing::info!(
            cwd = %cwd.display(),
            elapsed_ms = started.elapsed().as_millis(),
            events = summary.event_count,
            added = summary.added,
            removed = summary.removed,
            store_dir = summary.store_dir.as_deref(),
            "pnpm install completed"
        );
        return Ok(InstallOutcome {
            elapsed: started.elapsed(),
            summary,
        });
    }

    let combined = format!(
        "{}\n{}\n{}",
        stderr_result.tail,
        stdout_result.tail,
        stream_errors.join("\n")
    );
    let (kind, code, message) = classify_failure(&summary, &combined);
    tracing::warn!(
        cwd = %cwd.display(),
        exit_code = status.code().unwrap_or(-1),
        failure_kind = %kind,
        pnpm_code = code.as_deref(),
        events = summary.event_count,
        warnings = summary.warning_count,
        "pnpm install failed"
    );
    let code_suffix = code
        .as_deref()
        .map(|value| format!(", code {value}"))
        .unwrap_or_default();
    Err(InstallError::Failed {
        exit_code: status.code().unwrap_or(-1),
        kind,
        code,
        code_suffix,
        message,
        summary: Box::new(summary),
    })
}

async fn terminate_and_reap(child: &mut tokio::process::Child, pid: Option<u32>) {
    if let Some(pid) = pid {
        process::kill_process_group_force(pid);
    } else {
        let _ = child.start_kill();
    }
    // 回收子进程，让 stdout/stderr reader 收到 EOF，避免留下 zombie 和后台 task。
    let _ = child.wait().await;
}

#[derive(Default)]
struct StreamResult {
    summary: InstallSummary,
    tail: String,
    error: Option<String>,
}

async fn read_stream<R>(stream: R, logs: Option<&LogFiles>, parse_protocol: bool) -> StreamResult
where
    R: AsyncRead + Unpin,
{
    let mut result = StreamResult::default();
    let mut lines = BufReader::new(stream).lines();
    let mut write_logs = logs.is_some();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                result.error = Some(format!("read pnpm output failed: {error}"));
                break;
            }
        };
        push_bounded(&mut result.tail, &line);
        let observed = if parse_protocol {
            observe_event(&line, &mut result.summary)
        } else {
            ObservedLine::Unstructured
        };
        if write_logs && let Some(logs) = logs {
            let write_result = match observed {
                ObservedLine::Rendered(rendered) => write_log_line(logs, &rendered).await,
                ObservedLine::Unstructured => write_log_line(logs, &line).await,
                ObservedLine::Suppressed => Ok(()),
            };
            if let Err(error) = write_result {
                result.error = Some(format!("write pnpm log failed: {error}"));
                // 继续排空 child 管道并解析协议，但不再对每一行重复触发相同日志错误。
                write_logs = false;
            }
        }
    }
    result
}

async fn write_log_line(files: &LogFiles, line_value: &str) -> crate::error::AppResult<()> {
    let line_value = format!(
        "[{}] {}",
        Local::now().format("%Y/%m/%d %H:%M:%S"),
        line_value
    );
    log::append_line(&files.main, &line_value).await?;
    log::append_line(&files.temporary, &line_value).await
}

fn push_bounded(buffer: &mut String, line: &str) {
    buffer.push_str(line);
    buffer.push('\n');
    if buffer.len() > CAPTURE_LIMIT {
        let mut remove = buffer.len() - CAPTURE_LIMIT;
        while remove < buffer.len() && !buffer.is_char_boundary(remove) {
            remove += 1;
        }
        buffer.drain(..remove);
    }
}
