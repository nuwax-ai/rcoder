//! chat 命令实现
//!
//! 使用 AcpClientBuilder 启动 Agent，支持单次交互和交互式对话模式。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use agent_abstraction::{AcpClient, AcpClientBuilder, PromptCompletionSignal};
use tokio::signal::unix::{SignalKind, signal};

use crate::cli::ChatArgs;
use crate::diagnostics::TerminalDiagnosticsListener;
use crate::notifier::TerminalSessionNotifier;
use crate::output::{OutputFormatter, OutputLevel};
use crate::permission::TerminalPermissionPrompt;
use crate::registry::SimpleSessionRegistry;

/// Abort-on-drop wrapper for JoinHandle.
///
/// When dropped, aborts the underlying task. This prevents leaked blocking
/// tasks when Ctrl+C fires during stdin read.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, tokio::task::JoinError>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

/// 退出码定义
#[derive(Debug, Clone, Copy)]
#[repr(i32)]
#[allow(dead_code)]
pub enum ExitCode {
    /// 成功
    Success = 0,
    /// 一般错误
    GeneralError = 1,
    /// Agent 启动失败
    AgentStartFailed = 2,
    /// Prompt 发送/等待失败
    PromptFailed = 3,
    /// 超时
    Timeout = 4,
    /// 用户中断 (Ctrl+C)
    Interrupted = 130,
}

/// 构建 AcpClient
async fn build_client(
    args: &ChatArgs,
    level: OutputLevel,
    completion_signal: PromptCompletionSignal,
) -> Result<AcpClient<TerminalSessionNotifier, SimpleSessionRegistry>, anyhow::Error> {
    let notifier = TerminalSessionNotifier::new(
        OutputFormatter::new(level),
        Some(completion_signal.clone()),
    );
    let registry = SimpleSessionRegistry::new();
    let env_vars = args.common.parse_env_vars();
    let timeout = Duration::from_secs(args.common.timeout);
    let agent_mode = args.common.agent_mode();

    let diagnostics_listener = Arc::new(TerminalDiagnosticsListener::new(OutputFormatter::new(
        level,
    )));

    let mut builder = AcpClientBuilder::new(notifier, registry)
        .command(&args.common.command)
        .agent_id(&args.common.agent_id)
        .agent_mode(agent_mode)
        .timeout(timeout)
        .completion_signal(completion_signal)
        .diagnostics_listener(diagnostics_listener);

    // Interactive mode: inject TerminalPermissionPrompt when mode is "ask"
    if matches!(agent_mode, shared_types::AgentMode::Ask) {
        let prompt_handler = Arc::new(agent_abstraction::InteractivePermissionHandler::new(
            Arc::new(TerminalPermissionPrompt::new()),
        ));
        builder = builder.permission_handler(prompt_handler);
    }

    if let Some(ref working_dir) = args.common.working_dir {
        builder = builder.working_dir(working_dir.clone());
    }
    if let Some(ref project_id) = args.common.project_id {
        builder = builder.project_id(project_id.clone());
    }
    if let Some(ref system_prompt) = args.common.system_prompt {
        builder = builder.system_prompt(system_prompt.clone());
    }
    if !env_vars.is_empty() {
        builder = builder.envs(env_vars);
    }
    if !args.common.arg.is_empty() {
        builder = builder.args(args.common.arg.clone());
    }

    builder.start().await
}

/// 执行 chat 命令
pub async fn execute_chat(args: ChatArgs, verbose: u8, quiet: bool) -> ExitCode {
    let level = OutputLevel::from_verbose_count(verbose, quiet);
    let formatter = OutputFormatter::new(level);

    let is_interactive = args.prompt.is_none();
    let single_prompt = args.prompt.clone();

    formatter.info(&format!("启动 Agent: command={}", args.common.command));

    let completion_signal = PromptCompletionSignal::new();

    let client = match build_client(&args, level, completion_signal).await {
        Ok(client) => {
            formatter.success(&format!(
                "Agent 已启动: project_id={}, session_id={}",
                client.project_id(),
                client.session_id()
            ));
            client
        }
        Err(e) => {
            formatter.error(&format!("Agent 启动失败: {}", e));
            return ExitCode::AgentStartFailed;
        }
    };

    let exit_code = if is_interactive {
        run_interactive_loop(&client, &formatter).await
    } else {
        run_single_prompt(&client, &formatter, single_prompt.unwrap(), args.common.timeout).await
    };

    // 优雅停止 Agent
    formatter.debug("正在停止 Agent...");
    match client.stop().await {
        Ok(()) => formatter.debug("Agent 已停止"),
        Err(e) => formatter.warn(&format!("Agent 停止时出错: {}", e)),
    }

    exit_code
}

/// 单次 prompt 模式
async fn run_single_prompt(
    client: &AcpClient<TerminalSessionNotifier, SimpleSessionRegistry>,
    formatter: &OutputFormatter,
    prompt: String,
    timeout_secs: u64,
) -> ExitCode {
    formatter.separator();
    formatter.debug(&format!("发送 prompt: {}", prompt));

    match client.send_prompt_and_wait(&prompt).await {
        Ok(()) => {
            formatter.separator();
            formatter.success("Agent 响应完成");
            ExitCode::Success
        }
        Err(e) => {
            formatter.separator();
            let err_str = format!("{}", e);
            if err_str.contains("timed out") {
                formatter.error(&format!("请求超时 ({}s): {}", timeout_secs, e));
                ExitCode::Timeout
            } else {
                formatter.error(&format!("Prompt 失败: {}", e));
                ExitCode::PromptFailed
            }
        }
    }
}

/// 交互式对话循环
///
/// 使用 tokio signal 实现可复用的 Ctrl+C 处理：
/// - 用户输入 prompt → 发送给 agent → 等待响应 → 再次提示输入
/// - Ctrl+C 在等待输入时退出循环
/// - Ctrl+C 在 prompt 执行中触发 cancel()
/// - 输入 "exit" 或 "quit" 退出
async fn run_interactive_loop(
    client: &AcpClient<TerminalSessionNotifier, SimpleSessionRegistry>,
    formatter: &OutputFormatter,
) -> ExitCode {
    formatter.info("进入交互模式（输入 exit/quit 退出，Ctrl+C 中断）");
    formatter.separator();

    // Reusable SIGINT handler (unlike ctrl_c(), can fire multiple times)
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            formatter.error(&format!("无法注册 SIGINT 处理器: {}", e));
            return ExitCode::GeneralError;
        }
    };

    loop {
        // Print prompt
        eprint!("\x1b[36m\x1b[1m> \x1b[0m");

        // Read a line from stdin with abort-on-drop to prevent thread leak
        let line_handle = AbortOnDrop(tokio::task::spawn_blocking(|| {
            let mut input = String::new();
            match std::io::stdin().read_line(&mut input) {
                Ok(0) => Ok(None), // EOF
                Ok(_) => Ok(Some(input.trim().to_string())),
                Err(e) => Err(e),
            }
        }));

        // Wait for either input or Ctrl+C
        let input = tokio::select! {
            result = line_handle => {
                match result {
                    Ok(Ok(Some(line))) => line,
                    Ok(Ok(None)) => {
                        formatter.info("EOF，退出");
                        return ExitCode::Success;
                    }
                    Ok(Err(e)) => {
                        formatter.error(&format!("读取输入失败: {}", e));
                        return ExitCode::GeneralError;
                    }
                    Err(e) => {
                        // Task was aborted (e.g. by Ctrl+C in a prior iteration)
                        if e.is_cancelled() {
                            continue;
                        }
                        formatter.error(&format!("任务错误: {}", e));
                        return ExitCode::GeneralError;
                    }
                }
            }
            _ = sigint.recv() => {
                eprintln!();
                formatter.info("收到 Ctrl+C，退出");
                return ExitCode::Interrupted;
            }
        };

        // Handle special commands
        match input.to_lowercase().as_str() {
            "exit" | "quit" | ":q" => {
                formatter.info("退出");
                return ExitCode::Success;
            }
            "" => continue,
            _ => {}
        }

        // Send prompt and wait for response, with Ctrl+C → cancel()
        formatter.debug(&format!("发送: {}", input));
        tokio::select! {
            result = client.send_prompt_and_wait(&input) => {
                match result {
                    Ok(()) => {
                        formatter.separator();
                    }
                    Err(e) => {
                        formatter.error(&format!("Prompt 失败: {}", e));
                        // Continue the loop, don't exit on prompt failure in interactive mode
                    }
                }
            }
            _ = sigint.recv() => {
                formatter.warn("收到 Ctrl+C，正在取消当前操作...");
                if let Err(e) = client.cancel().await {
                    formatter.error(&format!("取消失败: {}", e));
                }
                // Continue the loop after cancellation
            }
        }
    }
}
