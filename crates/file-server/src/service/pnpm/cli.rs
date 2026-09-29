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
use crate::service::build_manager::BuildGuard;
use crate::service::dev_server::log;
use process_utils::command_context::{CommandContext, CommandRecord, retain_cleanup};

const CAPTURE_LIMIT: usize = 1024 * 1024;

/// install 入口（含 pnpm≥12 ignored-builds 自愈）。
///
/// pnpm 12 起 `--reporter=ndjson` 下被阻断的依赖构建脚本（ERR_PNPM_IGNORED_BUILDS,
/// 如 esbuild postinstall）会让 install 以 exit 1 失败（plain reporter 仅告警——
/// pnpm 12.4.1 实测）。自愈：解析被阻断的包名 → `pnpm approve-builds <pkgs>` 显式
/// 放行（等价用户交互确认，非 dangerously-allow-all-builds 全量放开）→ 重试一次。
/// pnpm <12 无此失败码，路径不触发。
pub(super) async fn install(
    cwd: &Path,
    options: &InstallOptions,
    logs: Option<&LogFiles>,
    timeout_secs: u64,
) -> Result<InstallOutcome, InstallError> {
    install_with_heal(cwd, options, logs, timeout_secs, true).await
}

/// 单次执行 + 失败自愈判定（heal 门控防递归：自愈重试后不再触发）。
async fn install_with_heal(
    cwd: &Path,
    options: &InstallOptions,
    logs: Option<&LogFiles>,
    timeout_secs: u64,
    heal_allowed: bool,
) -> Result<InstallOutcome, InstallError> {
    let result = install_once(cwd, options, logs, timeout_secs).await;
    if !heal_allowed {
        return result;
    }
    // R11：只保留 typed code 通道（classify.rs 从原始输出边界提取 code——
    // "边界解析一次，内部用结构化结果"；message 文案不再参与触发）
    let is_ignored_builds = matches!(
        &result,
        Err(InstallError::Failed { code, .. })
            if code.as_deref() == Some("ERR_PNPM_IGNORED_BUILDS")
    );
    if !is_ignored_builds {
        return result;
    }
    // 完整 "Ignored build scripts: a@1, b@2" 清单在 tail 里——Failed.message 只保留
    // 最后一行。双通道解析：先 message，空则读 install 主日志尾部（LogFiles.main
    // 已实时写入完整输出）。
    let from_message = result
        .as_ref()
        .err()
        .and_then(|e| match e {
            InstallError::Failed { message, .. } => Some(extract_ignored_build_packages(message)),
            _ => None,
        })
        .unwrap_or_default();
    let packages = if !from_message.is_empty() {
        from_message
    } else if let Some(logs) = logs
        && let Ok(tail) = read_tail(&logs.main, 64 * 1024).await
    {
        extract_ignored_build_packages(&tail)
    } else {
        Vec::new()
    };
    if packages.is_empty() {
        tracing::warn!(
            cwd = %cwd.display(),
            "pnpm ignored-builds failure carried no parseable package list; no self-heal"
        );
        return result;
    }
    tracing::info!(
        cwd = %cwd.display(),
        packages = ?packages,
        "pnpm ignored-builds: approving blocked build scripts and retrying install once"
    );
    approve_builds(cwd, &packages, timeout_secs).await?;
    // 自愈重试等价 install_once（heal_allowed=false 分支只透传单次结果，
    // 不再触发自愈——直调避免 async 递归 boxing）。
    install_once(cwd, options, logs, timeout_secs).await
}

/// 读文件尾部（自愈解析被阻断包名的兜底通道）。
async fn read_tail(path: &Path, limit: u64) -> std::io::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    let start = len.saturating_sub(limit);
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// 从失败 message 提取被阻断的包名（`Ignored build scripts: a@1.2.3, b@2` → [a, b]）。
fn extract_ignored_build_packages(message: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in message.lines() {
        // 日志管道会给行加 `[时间戳] ╰─▶ ` 前缀——用 contains 定位再取后段
        let Some(marker) = line.find("Ignored build scripts:") else {
            continue;
        };
        let rest = &line[marker + "Ignored build scripts:".len()..];
        for token in rest.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            // name@version → name（scoped 包 @scope/name@version 取第一个 @ 前段以外全部）
            let name = match token.rsplit_once('@') {
                Some((head, version))
                    if !head.is_empty()
                        && version.chars().all(|c| c.is_ascii_digit() || c == '.') =>
                {
                    head
                }
                _ => token,
            };
            if !name.is_empty() && !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// 显式放行被阻断的构建脚本（非交互 `pnpm approve-builds <pkgs>`；best-effort，
/// 失败仅告警——随后重试的 install 会给出真实失败原因）。
async fn approve_builds(
    cwd: &Path,
    packages: &[String],
    timeout_secs: u64,
) -> Result<(), InstallError> {
    let mut command = Command::new("pnpm");
    command
        .arg("approve-builds")
        .args(packages)
        .current_dir(cwd);
    command.env_remove("CI");
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    match run_owned_command(command, None, timeout_secs).await {
        Ok((status, _, _)) if status.success() => Ok(()),
        Err(error @ (InstallError::Cancelled | InstallError::CleanupUnconfirmed { .. })) => {
            Err(error)
        }
        result => {
            // Ordinary approve failures remain best-effort; the next install
            // provides the diagnostic. Cancellation must never start a retry.
            tracing::warn!(
                ?result,
                "pnpm approve-builds failed; retry install will surface error"
            );
            Ok(())
        }
    }
}

async fn install_once(
    cwd: &Path,
    options: &InstallOptions,
    logs: Option<&LogFiles>,
    timeout_secs: u64,
) -> Result<InstallOutcome, InstallError> {
    let args = install_args(options);

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
    let started = Instant::now();
    let (status, stdout_result, stderr_result) =
        run_owned_command(command, logs, timeout_secs).await?;
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

/// 构造实际安装参数（install_once 的单一来源，测试同一函数）。
///
/// 业务策略：开发安装允许 lockfile 创建/更新——项目无 pnpm-lock.yaml（脚手架新
/// 建项目）或 package.json 变更后，安装应生成/同步 lockfile 而不是以
/// ERR_PNPM_NO_LOCKFILE / ERR_PNPM_OUTDATED_LOCKFILE 失败。默认值放在 extra_args
/// 之前，显式传入的参数仍可覆盖。
fn install_args(options: &InstallOptions) -> Vec<String> {
    let mut args = vec!["--reporter=ndjson".to_string(), "install".to_string()];
    args.push("--no-frozen-lockfile".to_string());
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
    args
}

/// All pnpm phases share command cancellation and process-tree ownership with
/// manifest builds, including the lockfile/ignored-builds recovery branches.
async fn run_owned_command(
    command: Command,
    logs: Option<&LogFiles>,
    timeout_secs: u64,
) -> Result<(std::process::ExitStatus, StreamResult, StreamResult), InstallError> {
    let cancellation = CommandContext::current()
        .map(|context| context.cancellation)
        .unwrap_or_default();
    if cancellation.is_cancelled() {
        return Err(InstallError::Cancelled);
    }
    if BuildGuard::current().is_some_and(|guard| guard.cleanup_pending()) {
        return Err(InstallError::CleanupUnconfirmed {
            reason: "the previous command in this build is still being stopped".into(),
        });
    }
    let record = CommandRecord::prepare().map_err(|source| InstallError::Wait { source })?;
    let mut child = match process_utils::guardian::spawn_owned(command, record.as_ref()).await {
        Ok(child) => child,
        Err(error) => {
            if let Some(record) = record
                && record.quiescent().is_err()
            {
                retain_cleanup(None, Some(record));
            }
            return Err(InstallError::Spawn {
                source: std::io::Error::other(error),
            });
        }
    };
    if let Some(receipt) = &record
        && let Err(source) = receipt.running(child.id())
    {
        BuildGuard::retain_command_cleanup(child, record);
        return Err(InstallError::CleanupUnconfirmed {
            reason: format!("persist running receipt failed: {source}; cleanup continues"),
        });
    }
    let stdout = child.take_stdout();
    let stderr = child.take_stderr();
    let stdout_logs = logs.cloned();
    let stderr_logs = logs.cloned();
    let stdout_task = tokio::spawn(async move {
        match stdout {
            Some(stream) => read_stream(stream, stdout_logs.as_ref()).await,
            None => StreamResult::default(),
        }
    });
    let stderr_task = tokio::spawn(async move {
        match stderr {
            Some(stream) => read_stream(stream, stderr_logs.as_ref()).await,
            None => StreamResult::default(),
        }
    });
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(InstallError::Cancelled),
        waited = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_root()) => {
            match waited {
                Ok(result) => result.map_err(|source| InstallError::Wait { source }),
                Err(_) => Err(InstallError::TimedOut { timeout_secs }),
            }
        }
    };
    let result = if matches!(
        child.stop(Duration::from_secs(1)).await,
        process_utils::managed_tree::StopOutcome::Unconfirmed
    ) {
        BuildGuard::retain_command_cleanup(child, record);
        Err(InstallError::CleanupUnconfirmed {
            reason: "bounded process-tree stop did not finish; cleanup continues".into(),
        })
    } else if let Some(record) = record {
        match record.quiescent() {
            Ok(()) => result,
            Err(source) => {
                retain_cleanup(None, Some(record));
                Err(InstallError::Wait { source })
            }
        }
    } else {
        result
    };
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let stdout = finish_stream(stdout_task, drain_deadline).await;
    let stderr = finish_stream(stderr_task, drain_deadline).await;
    result.map(|status| (status, stdout, stderr))
}

async fn finish_stream(
    mut task: tokio::task::JoinHandle<StreamResult>,
    deadline: tokio::time::Instant,
) -> StreamResult {
    match tokio::time::timeout_at(deadline, &mut task).await {
        Ok(Ok(result)) => result,
        result => {
            task.abort();
            let error = format!("pnpm output drain incomplete: {result:?}");
            tracing::warn!(%error);
            StreamResult {
                error: Some(error),
                ..StreamResult::default()
            }
        }
    }
}

#[derive(Debug, Default)]
struct StreamResult {
    summary: InstallSummary,
    tail: String,
    error: Option<String>,
}

/// 逐行读取 pnpm 的 stdout/stderr, 解析 NDJSON 协议事件并按需落盘。
///
/// pnpm `--reporter=ndjson` 的输出分流: **正常事件** (scope/manifest/progress/link/
/// hoist/stats/summary, 多为 debug 级) 走 **stdout**, 仅 `ERR_PNPM_*` 走 **stderr**。
/// 故两流都必须经 observe_event 解析: 高频 debug 事件被 Suppressed 不落盘, 有用事件
/// Rendered, 真错误计入 summary.error_codes/diagnostics 供失败分类。若只解析其中一流,
/// 另一流的原始 JSON 会全部落盘 → 海量 debug 噪音 (实测单次 install 可产生 4000+ 行)。
async fn read_stream<R>(stream: R, logs: Option<&LogFiles>) -> StreamResult
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
        let observed = observe_event(&line, &mut result.summary);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_args_default_allows_lockfile_updates() {
        let args = install_args(&InstallOptions::default());
        assert_eq!(
            args,
            vec![
                "--reporter=ndjson",
                "install",
                "--no-frozen-lockfile",
                "--config.confirmModulesPurge=false",
            ]
        );
    }

    #[test]
    fn install_args_prefer_offline_kept() {
        let args = install_args(&InstallOptions::prefer_offline());
        assert_eq!(
            args,
            vec![
                "--reporter=ndjson",
                "install",
                "--no-frozen-lockfile",
                "--prefer-offline",
                "--config.confirmModulesPurge=false",
            ]
        );
    }

    #[test]
    fn install_args_extra_args_appended_after_defaults() {
        let options = InstallOptions {
            prefer_offline: false,
            extra_args: vec!["--config.production=false".to_string()],
        };
        let args = install_args(&options);
        assert_eq!(
            args.last().map(String::as_str),
            Some("--config.production=false")
        );
        let no_frozen = args
            .iter()
            .position(|a| a == "--no-frozen-lockfile")
            .expect("default --no-frozen-lockfile present");
        let first_extra = args.len() - 1;
        assert!(no_frozen < first_extra, "defaults precede extra_args");
        assert!(args.iter().all(|a| a != "--frozen-lockfile"));
    }
}
