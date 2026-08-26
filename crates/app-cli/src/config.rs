//! CLI 参数（clap）。
//!
//! 三种形态：
//! - `app-cli serve`：**容器 server 形态**（supervisord [program:app-cli] 的 command）——
//!   常驻状态机（Idle→Deploying→Orchestrating→Running），无论是否部署都在，
//!   管理 API + 探针 + 热部署端点 + 服务编排；
//! - `app-cli run-service <id>`：单服务包装（supervisord 动态 program 的 command）——
//!   读 server 写下的 spec 文件，组装 env 后 exec 服务本体；
//! - **无子命令**：legacy 直跑形态（file-server dev 链 spawn 的兼容入口）——
//!   deploy 段 → idle/api/supervisor 一次性编排，行为与 serve 演化前完全一致。

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// UserApp 容器运行时编排器。
#[derive(Parser, Debug, Clone)]
#[command(
    name = "app-cli",
    version,
    about = "UserApp 容器运行时编排器（替代 start.sh）"
)]
pub struct CliArgs {
    /// workspace 根（含 workspace.manifest.toml + 各子项目；解压后的 /app/code）。
    #[arg(long, default_value = "/app/code", env = "APP_CLI_WORKSPACE")]
    pub workspace: PathBuf,

    /// 日志目录（按子项目分文件：`<project>.{out,err}.log`）。
    #[arg(long, default_value = "/app/logs", env = "APP_CLI_LOG_DIR")]
    pub log_dir: PathBuf,

    /// 管理 API 监听地址。
    #[arg(long, default_value = "0.0.0.0:3010", env = "APP_CLI_ADMIN_ADDR")]
    pub admin_addr: String,

    /// pingap 二进制路径。
    #[arg(
        long,
        default_value = "/usr/local/bin/pingap",
        env = "APP_CLI_PINGAP_BIN"
    )]
    pub pingap_bin: PathBuf,

    /// 本地开发：只为 <WORKSPACE> 生成 release.lock.toml + 预览 Pingap 生效配置后退出
    /// （不启动服务、不依赖 pingap 二进制 / PG）。供 manifest/路由设计秒级迭代验证。
    #[arg(long, value_name = "WORKSPACE", env = "APP_CLI_GEN_LOCK")]
    pub gen_lock: Option<PathBuf>,

    /// 子命令（缺省 = legacy 直跑形态，dev 链兼容入口）。
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// 常驻 server：探针 + 管理 API + 热部署端点 + 服务编排（容器形态）。
    Serve,

    /// 单服务进程包装（supervisord 动态 program 的 command）：读 spec → exec 服务本体。
    RunService {
        /// 服务 ID（对应 release.lock.services[].service_id）。
        #[arg(value_name = "SERVICE_ID")]
        service_id: String,
    },
}
