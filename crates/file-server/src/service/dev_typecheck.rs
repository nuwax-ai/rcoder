//! web/computer 开发启动前检查；仅真实 TS 诊断阻断，环境问题有证据地放行。
use super::dev_server::{
    log,
    process::{self, CommandCompletion, CommandObservers},
};
use crate::error::{AppError, AppResult};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const TYPECHECK_TIMEOUT_SECS: u64 = 180;
const MAX_OUTPUT_CHARS: usize = 6000;

#[derive(Default)]
struct CapturedOutput {
    text: String,
    captured_chars: usize,
    total_chars: usize,
    ts_diagnostics: bool,
}
impl CapturedOutput {
    fn line(&mut self, line: &str) {
        self.ts_diagnostics |= has_ts_diagnostics(line);
        self.total_chars = self.total_chars.saturating_add(line.chars().count() + 1);
        let remaining = MAX_OUTPUT_CHARS.saturating_sub(self.captured_chars);
        let part: String = line
            .chars()
            .chain(std::iter::once('\n'))
            .take(remaining)
            .collect();
        self.captured_chars += part.chars().count();
        self.text.push_str(&part);
    }
    fn text(&self) -> String {
        let text = self.text.trim();
        if self.total_chars > MAX_OUTPUT_CHARS {
            format!(
                "{text}\n... (output truncated, total {} chars)",
                self.total_chars
            )
        } else {
            text.to_string()
        }
    }
}
fn has_ts_diagnostics(text: &str) -> bool {
    text.match_indices("error TS").any(|(start, _)| {
        let rest = &text[start + "error TS".len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || rest.as_bytes().get(digits) != Some(&b':') {
            return false;
        }
        // 微软 TypeScript v5.9.3 diagnosticMessages.json 中明确的参数/配置诊断；
        // 不按 5xxx 或 180xx 家族忽略（TS5075、TS18047 是真实源码错误）。
        // https://raw.githubusercontent.com/microsoft/TypeScript/v5.9.3/src/compiler/diagnosticMessages.json
        !matches!(
            rest[..digits].parse::<u32>().ok(),
            Some(
                5023 | 5024
                    | 5025
                    | 5042
                    | 5057
                    | 5058
                    | 5072
                    | 5073
                    | 5077
                    | 18000
                    | 18002
                    | 18003
            )
        )
    })
}

struct CheckPlan {
    args: Vec<&'static str>,
}
impl CheckPlan {
    fn command(&self) -> String {
        format!("pnpm {}", self.args.join(" "))
    }
}
async fn choose(cwd: &Path) -> Option<CheckPlan> {
    let bytes = tokio::fs::read(cwd.join("package.json")).await.ok()?;
    let package: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let present = |name: &'static str| async move {
        let base = cwd.join("node_modules/.bin").join(name);
        if tokio::fs::try_exists(&base).await.unwrap_or(false) {
            return true;
        }
        #[cfg(windows)]
        for suffix in ["cmd", "exe", "bat"] {
            if tokio::fs::try_exists(base.with_extension(suffix))
                .await
                .unwrap_or(false)
            {
                return true;
            }
        }
        false
    };
    if package
        .get("scripts")
        .and_then(|scripts| scripts.get("typecheck"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|script| !script.trim().is_empty())
    {
        return Some(CheckPlan {
            args: vec!["run", "typecheck"],
        });
    }
    if tokio::fs::try_exists(cwd.join("turbo.json"))
        .await
        .unwrap_or(false)
        && present("turbo").await
    {
        return Some(CheckPlan {
            args: vec!["exec", "turbo", "run", "typecheck"],
        });
    }
    if tokio::fs::try_exists(cwd.join("tsconfig.json"))
        .await
        .unwrap_or(false)
    {
        if present("vue-tsc").await {
            return Some(CheckPlan {
                args: vec!["exec", "vue-tsc", "--noEmit"],
            });
        }
        if present("tsc").await {
            return Some(CheckPlan {
                args: vec!["exec", "tsc", "--noEmit"],
            });
        }
    }
    None
}

/// 先解析实际 pnpm，缺工具无需启动新进程；路径通过 argv 使用。
async fn pnpm_program(cwd: &Path) -> Option<PathBuf> {
    let mut directories = vec![cwd.join("node_modules/.bin")];
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path));
    }
    for directory in directories {
        #[cfg(windows)]
        let names = ["pnpm.exe", "pnpm.cmd", "pnpm.bat", "pnpm"];
        #[cfg(not(windows))]
        let names = ["pnpm"];
        for name in names {
            let candidate = directory.join(name);
            if let Ok(metadata) = tokio::fs::metadata(&candidate).await
                && metadata.is_file()
            {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    if metadata.permissions().mode() & 0o111 == 0 {
                        continue;
                    }
                }
                return Some(candidate);
            }
        }
    }
    None
}

pub(crate) async fn run(
    cwd: &Path,
    project_id: &str,
    main_log: &Path,
    temp_log: &Path,
) -> AppResult<()> {
    run_with_timeout(cwd, project_id, main_log, temp_log, TYPECHECK_TIMEOUT_SECS).await
}
async fn run_with_timeout(
    cwd: &Path,
    project_id: &str,
    main_log: &Path,
    temp_log: &Path,
    timeout_secs: u64,
) -> AppResult<()> {
    let Some(plan) = choose(cwd).await else {
        tracing::debug!(
            project_id,
            "typecheck skipped: no package or local TypeScript check"
        );
        return Ok(());
    };
    let Some(program) = pnpm_program(cwd).await else {
        tracing::warn!(project_id, "typecheck skipped: pnpm is unavailable");
        return Ok(());
    };
    let Some(program) = program.to_str() else {
        tracing::warn!(
            project_id,
            "typecheck skipped: pnpm path cannot be represented"
        );
        return Ok(());
    };
    let command = plan.command();
    for path in [main_log, temp_log] {
        log::append_line(
            path,
            &format!("> dev 启动前类型检查（与 build 同一标准）: {command}"),
        )
        .await?;
    }
    let captured = Arc::new(Mutex::new(CapturedOutput::default()));
    let lines = captured.clone();
    let completion = Mutex::new(None);
    let completed = |value| {
        *completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
    };
    let result = process::run_command_to_log(
        program,
        &plan.args,
        cwd,
        main_log,
        temp_log,
        timeout_secs,
        CommandObservers {
            on_line: Some(Arc::new(move |line| {
                lines
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .line(line)
            })),
            on_completed: Some(&completed),
            ..Default::default()
        },
    )
    .await;
    if process_utils::command_context::CommandContext::current()
        .is_some_and(|context| context.cancellation.is_cancelled())
    {
        return Err(result
            .err()
            .unwrap_or_else(|| AppError::business("typecheck cancelled; dev was not started")));
    }
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let outcome = *completion
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let output = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match outcome {
                Some(CommandCompletion::TimedOut | CommandCompletion::NotStarted) => {
                    tracing::warn!(
                        project_id,
                        ?outcome,
                        "typecheck environment failure passed after physical cleanup"
                    );
                    Ok(())
                }
                Some(CommandCompletion::Exited {
                    output_complete: true,
                    ..
                }) if !output.ts_diagnostics => {
                    tracing::warn!(
                        project_id,
                        "typecheck tool/configuration failure passed after physical cleanup"
                    );
                    Ok(())
                }
                Some(CommandCompletion::Exited {
                    code,
                    output_complete: true,
                }) => {
                    let raw = output.text();
                    Err(AppError::business_with_details(
                        format!(
                            "开发服务器启动前 TypeScript 类型检查未通过（与 build 同一标准），请修复后重试：\n{raw}"
                        ),
                        serde_json::json!({
                            "code":"TYPECHECK_FAILED", "command": command, "exitCode": code,
                            "suggestion":super::build_error::parse(&raw),
                        }),
                    ))
                }
                _ => Err(error),
            }
        }
    }
}

/// web 构建使用原始两管输出；只把已确认的脚本非零退出转为业务 400。
pub(crate) async fn run_build_script(
    program: &str,
    args: &[&str],
    cwd: &Path,
    main_log: &Path,
    temp_log: &Path,
    timeout_secs: u64,
) -> AppResult<()> {
    let captured = Arc::new(Mutex::new(CapturedOutput::default()));
    let lines = captured.clone();
    let completion = Mutex::new(None);
    let completed = |value| {
        *completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
    };
    let result = process::run_command_to_log(
        program,
        args,
        cwd,
        main_log,
        temp_log,
        timeout_secs,
        CommandObservers {
            on_line: Some(Arc::new(move |line| {
                lines
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .line(line)
            })),
            on_completed: Some(&completed),
            ..Default::default()
        },
    )
    .await;
    match result {
        Err(error)
            if matches!(
                *completion
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                Some(CommandCompletion::Exited { .. })
            ) =>
        {
            let output = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .text();
            let output = if output.is_empty() {
                error.to_string().chars().take(MAX_OUTPUT_CHARS).collect()
            } else {
                output
            };
            let friendly = super::build_error::parse(&output);
            Err(AppError::business_with_details(
                format!("{friendly}\n{output}"),
                serde_json::json!({
                    "originalError":error.to_string().chars().take(MAX_OUTPUT_CHARS).collect::<String>(), "command":format!("{program} {}", args.join(" ")),
                }),
            ))
        }
        other => other,
    }
}

#[cfg(test)]
mod tests;
