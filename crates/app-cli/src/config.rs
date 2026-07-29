//! CLI 参数（clap）。

use std::path::PathBuf;

use clap::Parser;

/// UserApp 容器运行时编排器。
#[derive(Parser, Debug)]
#[command(name = "app-cli", version, about = "UserApp 容器运行时编排器（替代 start.sh）")]
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
    #[arg(long, default_value = "/usr/local/bin/pingap", env = "APP_CLI_PINGAP_BIN")]
    pub pingap_bin: PathBuf,
}
