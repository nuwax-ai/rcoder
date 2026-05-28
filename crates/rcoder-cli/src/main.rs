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

use clap::Parser;

use crate::cli::{Cli, Commands};
use crate::commands::execute_chat;

/// 根据 verbose 级别设置日志过滤器
fn setup_logging(verbose: u8) {
    let filter = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    setup_logging(cli.verbose);

    let exit_code = match cli.command {
        Commands::Chat(args) => execute_chat(args, cli.verbose, cli.quiet).await,
        Commands::Tui(args) => tui::execute_tui(args, cli.verbose, cli.quiet).await,
    };

    std::process::exit(exit_code as i32);
}
