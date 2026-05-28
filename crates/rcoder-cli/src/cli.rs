//! CLI 参数定义
//!
//! 使用 clap derive 宏定义命令行参数结构。

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// rcoder-cli — ACP Agent 本地调试工具
///
/// 通过 AcpClientBuilder 在本地启动并调试 ACP 兼容的 Agent，
/// 无需 Docker 容器或 agent_runner 服务。
#[derive(Parser, Debug)]
#[command(
    name = "rcoder-cli",
    version,
    about = "ACP Agent 本地调试工具",
    long_about = "通过 AcpClientBuilder 在本地启动并调试 ACP 兼容的 Agent。\n\
                  支持单次交互和交互式对话模式。"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// 启用详细日志输出（可叠加：-v, -vv, -vvv）
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// 静默模式，仅输出 agent 响应内容
    #[arg(short, long, global = true)]
    pub quiet: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// 启动 Agent 并发送聊天请求（行内模式）
    Chat(ChatArgs),

    /// 启动 Agent 并进入全屏 TUI 交互界面
    Tui(TuiArgs),
}

/// Agent 启动公共参数
///
/// chat 和 tui 子命令共享的 Agent 配置参数。
/// 通过 `#[command(flatten)]` 内联到子命令中。
#[derive(Parser, Debug, Clone)]
pub struct CommonArgs {
    /// Agent 启动命令（如 "python", "./my-agent", "codex-acp"）
    #[arg(short, long)]
    pub command: String,

    /// Agent 工作目录
    #[arg(short = 'w', long)]
    pub working_dir: Option<PathBuf>,

    /// 项目 ID（不指定则自动生成 UUID）
    #[arg(long)]
    pub project_id: Option<String>,

    /// Agent 标识符
    #[arg(long, default_value = "custom-agent")]
    pub agent_id: String,

    /// 传递给 Agent 子进程的环境变量（格式：KEY=VALUE，可多次指定）
    #[arg(short, long, value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// 传递给 Agent 命令的额外参数（可多次指定）
    #[arg(long)]
    pub arg: Vec<String>,

    /// Agent 权限模式：yolo（自动批准）或 ask（交互确认）
    #[arg(long, default_value = "yolo")]
    pub mode: String,

    /// 自定义系统提示词（追加到 Agent 默认提示词之后）
    #[arg(long)]
    pub system_prompt: Option<String>,

    /// 单次请求超时时间（秒）
    #[arg(long, default_value = "300")]
    pub timeout: u64,
}

impl CommonArgs {
    /// 解析环境变量参数（KEY=VALUE 格式）为 HashMap
    pub fn parse_env_vars(&self) -> std::collections::HashMap<String, String> {
        self.env
            .iter()
            .filter_map(|s| {
                let mut parts = s.splitn(2, '=');
                match (parts.next(), parts.next()) {
                    (Some(k), Some(v)) => Some((k.to_string(), v.to_string())),
                    _ => {
                        eprintln!(
                            "Warning: ignoring malformed env var '{}', expected KEY=VALUE",
                            s
                        );
                        None
                    }
                }
            })
            .collect()
    }

    /// 解析 agent 模式
    ///
    /// 支持 "yolo"（默认）和 "ask"。未知值打印警告并回退到 Yolo。
    pub fn agent_mode(&self) -> shared_types::AgentMode {
        match self.mode.to_lowercase().as_str() {
            "ask" => shared_types::AgentMode::Ask,
            "yolo" => shared_types::AgentMode::Yolo,
            other => {
                eprintln!(
                    "Warning: unknown agent mode '{}', falling back to 'yolo'",
                    other
                );
                shared_types::AgentMode::Yolo
            }
        }
    }
}

/// chat 子命令参数
#[derive(Parser, Debug)]
pub struct ChatArgs {
    /// Agent 启动公共参数
    #[command(flatten)]
    pub common: CommonArgs,

    /// 要发送给 Agent 的提示词（不指定则进入交互模式）
    #[arg(short, long)]
    pub prompt: Option<String>,
}

/// tui 子命令参数
///
/// 全屏 TUI 模式，对标 codex 的终端交互界面。
/// 复用 CommonArgs 的 Agent 启动参数，额外增加 TUI 专属选项。
#[derive(Parser, Debug)]
pub struct TuiArgs {
    /// Agent 启动公共参数
    #[command(flatten)]
    pub common: CommonArgs,

    /// 禁用 Markdown 渲染（纯文本模式）
    #[arg(long)]
    pub no_markdown: bool,
}
