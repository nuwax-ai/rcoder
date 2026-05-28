//! TUI 模式入口
//!
//! `rcoder-cli tui` 子命令 — 全屏终端交互界面。
//! 使用 ratatui 渲染多面板布局，支持流式 Markdown、权限弹窗、滚动查看。

pub mod app;
pub mod chat;
pub mod composer;
pub mod event;
pub mod markdown;
pub mod notifier;
pub mod permission;
pub mod terminal;
pub mod ui;

use std::sync::Arc;
use std::time::Duration;

use agent_abstraction::{AcpClientBuilder, DiagnosticsListener, ProcessDiagnostics, PromptCompletionSignal};
use tokio::sync::mpsc;

use crate::cli::TuiArgs;
use crate::commands::chat::ExitCode;
use crate::registry::SimpleSessionRegistry;

use app::{App, Client};
use event::AppEvent;
use notifier::TuiSessionNotifier;
use permission::TuiPermissionPrompt;

/// TUI 诊断监听器
///
/// 将 DiagnosticsListener 的同步回调通过 channel 转发到 TUI 事件循环。
struct TuiDiagnosticsListener {
    tx: mpsc::UnboundedSender<AppEvent>,
}

impl TuiDiagnosticsListener {
    fn new(tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        Self { tx }
    }
}

impl DiagnosticsListener for TuiDiagnosticsListener {
    fn on_process_started(&self, pid: u32, command: &str) {
        let _ = self.tx.send(AppEvent::Diagnostics(format!(
            "Agent 进程已启动: pid={}, command={}",
            pid, command
        )));
    }

    fn on_acp_initialized(&self, session_id: &str) {
        let _ = self.tx.send(AppEvent::Diagnostics(format!(
            "ACP 初始化完成: session_id={}",
            session_id
        )));
    }

    fn on_process_exited(&self, diagnostics: &ProcessDiagnostics) {
        if diagnostics.exit_code == Some(0) {
            let _ = self.tx.send(AppEvent::Diagnostics(
                "Agent 进程正常退出".to_string(),
            ));
        } else {
            let _ = self.tx.send(AppEvent::Diagnostics(format!(
                "Agent 进程异常退出: exit_code={:?}",
                diagnostics.exit_code
            )));
        }
    }

    fn on_process_error(&self, diagnostics: &ProcessDiagnostics) {
        let _ = self.tx.send(AppEvent::Diagnostics(format!(
            "Agent 进程错误: {}",
            diagnostics.error_message.as_deref().unwrap_or("unknown")
        )));
    }
}

/// 构建 TUI 模式的 AcpClient
async fn build_tui_client(
    args: &TuiArgs,
    event_tx: mpsc::UnboundedSender<AppEvent>,
    completion_signal: PromptCompletionSignal,
) -> Result<Client, anyhow::Error> {
    let notifier = TuiSessionNotifier::new(event_tx.clone(), Some(completion_signal.clone()));
    let registry = SimpleSessionRegistry::new();
    let timeout = Duration::from_secs(args.common.timeout);
    let agent_mode = args.common.agent_mode();

    let diagnostics_listener = Arc::new(TuiDiagnosticsListener::new(event_tx.clone()));

    let mut builder = AcpClientBuilder::new(notifier, registry)
        .command(&args.common.command)
        .agent_id(&args.common.agent_id)
        .agent_mode(agent_mode)
        .timeout(timeout)
        .completion_signal(completion_signal)
        .diagnostics_listener(diagnostics_listener);

    // Ask 模式下注入 TUI 权限弹窗
    if matches!(agent_mode, shared_types::AgentMode::Ask) {
        let prompt_handler = Arc::new(agent_abstraction::InteractivePermissionHandler::new(
            Arc::new(TuiPermissionPrompt::new(event_tx)),
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
    let env_vars = args.common.parse_env_vars();
    if !env_vars.is_empty() {
        builder = builder.envs(env_vars);
    }
    if !args.common.arg.is_empty() {
        builder = builder.args(args.common.arg.clone());
    }

    builder.start().await
}

/// 执行 tui 子命令
pub async fn execute_tui(args: TuiArgs, verbose: u8, quiet: bool) -> ExitCode {
    let use_markdown = !args.no_markdown;

    // 创建事件通道
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    // 创建完成信号
    let completion_signal = PromptCompletionSignal::new();

    // 先构建 Agent 客户端（在普通终端模式下，失败时错误直接输出到 stderr）
    let client = match build_tui_client(&args, event_tx.clone(), completion_signal).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Agent 启动失败: {}", e);
            return ExitCode::AgentStartFailed;
        }
    };

    // 客户端就绪后再初始化终端（进入 alt screen + raw mode）
    let tui_terminal = match terminal::init() {
        Ok(t) => t,
        Err(e) => {
            // 终端初始化失败，必须先停止 Agent 进程再退出
            if let Err(stop_err) = client.stop().await {
                eprintln!("Agent 停止时出错: {}", stop_err);
            }
            eprintln!("终端初始化失败: {}", e);
            return ExitCode::GeneralError;
        }
    };

    // 创建并运行 TUI 应用
    let client = Arc::new(client);
    let app = App::new(client, event_tx, event_rx, tui_terminal, use_markdown, verbose, quiet);
    let exit_code = app.run().await;

    if exit_code == 0 {
        ExitCode::Success
    } else if exit_code == 130 {
        ExitCode::Interrupted
    } else {
        ExitCode::GeneralError
    }
}
