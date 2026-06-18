//! rcoder-cli — ACP Agent 本地调试工具
//!
//! 通过 AcpClientBuilder 在本地启动并调试 ACP 兼容的 Agent，
//! 无需 Docker 容器或 agent_runner 服务。
//!
//! # 用法
//!
//! ```bash
//! # 行内模式 — 单次交互
//! rcoder-cli chat -c "./my-agent" -p "hello"
//!
//! # 行内模式 — 交互式
//! rcoder-cli chat -c "./my-agent"
//!
//! # TUI 模式 — 全屏终端交互
//! rcoder-cli tui -c "./my-agent"
//!
//! # 指定工作目录和环境变量
//! rcoder-cli tui -c "codex-acp" -w /path/to/project -e API_KEY=xxx
//! ```

mod cli;
mod commands;
mod diagnostics;
mod notifier;
mod output;
mod permission;
mod registry;
mod tui;

use std::path::Path;

use clap::Parser;
use tracing_appender::non_blocking::WorkerGuard;

use crate::cli::{Cli, Commands};
use crate::commands::execute_chat;

/// 根据 verbose 级别设置日志过滤器
///
/// 当指定 `log_file` 时，日志同时输出到 stderr（带颜色）和文件（无颜色，带 target）。
/// 返回 `WorkerGuard`，必须在 main 中保持存活以确保日志刷新。
fn setup_logging(verbose: u8, log_file: Option<&Path>) -> Option<WorkerGuard> {
    let filter = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter));

    match log_file {
        Some(path) => {
            let dir = path.parent().unwrap_or_else(|| Path::new("."));
            let file_name = path
                .file_name()
                .unwrap_or_else(|| panic!("Invalid log file path: {:?}", path));
            let file_appender = tracing_appender::rolling::never(dir, file_name);
            let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

            use tracing_subscriber::prelude::*;
            let stderr_layer = tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_writer(std::io::stderr);
            let file_layer = tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(non_blocking);
            tracing_subscriber::registry()
                .with(env_filter)
                .with(stderr_layer)
                .with(file_layer)
                .init();

            Some(guard)
        }
        None => {
            tracing_subscriber::fmt()
                .with_env_filter(env_filter)
                .with_target(false)
                .with_writer(std::io::stderr)
                .init();
            None
        }
    }
}

#[tokio::main]
async fn main() {
    // 拦截 --version/-V，只输出版本号（不含 binary name）
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

    let cli = Cli::parse();
    let guard = setup_logging(cli.verbose, cli.log_file.as_deref());

    let exit_code = match cli.command {
        Commands::Chat(args) => execute_chat(args, cli.verbose, cli.quiet).await,
        Commands::Tui(args) => tui::execute_tui(args, cli.verbose, cli.quiet).await,
    };

    // 显式 drop guard，确保 non_blocking writer 缓冲区刷新到文件
    // std::process::exit 不会运行析构函数，必须在 exit 前手动 drop
    drop(guard);
    std::process::exit(exit_code as i32);
}
