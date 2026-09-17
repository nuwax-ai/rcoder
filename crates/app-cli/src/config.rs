//! CLI 参数（clap）。
//!
//! 四种形态：
//! - `app-cli serve`：**容器 server 形态**（supervisord [program:app-cli] 的 command）——
//!   常驻状态机（Idle→Deploying→Orchestrating→Running），无论是否部署都在，
//!   管理 API + 探针 + 热部署端点 + 服务编排；
//! - `app-cli build`：**本地编译工具**——逐服务执行编译命令（--dev 三分派）+
//!   artifact 校验 +（可选）产物态部署布局组装；与 `--gen-lock`、`serve` 组成
//!   本地三步闭环（无平台环境的可构建性/可运行性验证）；
//! - `app-cli run-service <id>`：单服务包装（supervisord 动态 program 的 command）——
//!   读 server 写下的 spec 文件，组装 env 后 exec 服务本体；
//! - **无子命令**：legacy 直跑形态（file-server dev 链 spawn 的兼容入口）——
//!   deploy 段 → idle/api/supervisor 一次性编排，行为与 serve 演化前完全一致。

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Userapp 容器运行时编排器。
#[derive(Parser, Debug, Clone, Default)]
#[command(
    name = "app-cli",
    version,
    about = "Userapp 容器运行时编排器（替代 start.sh）"
)]
pub struct CliArgs {
    /// workspace 根（含 workspace.manifest.toml + 各子项目；解压后的 /app/code）。
    ///
    /// `global`：全局运行参数既可放顶层（`app-cli --workspace X serve`）也可
    /// 跟在子命令后（`app-cli serve --workspace X`）——后者是主流 CLI 直觉
    /// 形态（docker run --name / git commit -m 同构），B01 修复前的
    /// `serve --workspace` 解析期 exit 2 即因缺 global 声明。
    #[arg(
        long,
        global = true,
        default_value = "/app/code",
        env = "APP_CLI_WORKSPACE"
    )]
    pub workspace: PathBuf,

    /// 日志目录（按子项目分文件：`<project>.{out,err}.log`）。
    #[arg(
        long,
        global = true,
        default_value = "/app/logs",
        env = "APP_CLI_LOG_DIR"
    )]
    pub log_dir: PathBuf,

    /// 管理 API 监听地址。
    #[arg(
        long,
        global = true,
        default_value = "0.0.0.0:3010",
        env = "APP_CLI_ADMIN_ADDR"
    )]
    pub admin_addr: String,

    /// pingap 二进制路径。
    #[arg(
        long,
        global = true,
        default_value = "/usr/local/bin/pingap",
        env = "APP_CLI_PINGAP_BIN"
    )]
    pub pingap_bin: PathBuf,

    /// 本地开发：只为 <WORKSPACE> 生成 release.lock.toml + 预览 Pingap 生效配置后退出
    /// （不启动服务、不依赖 pingap 二进制 / PG）。供 manifest/路由设计秒级迭代验证。
    /// （顶层专属动作开关——不是运行配置，不 global：`serve --gen-lock` 无语义。）
    #[arg(long, value_name = "WORKSPACE", env = "APP_CLI_GEN_LOCK")]
    pub gen_lock: Option<PathBuf>,

    /// 子命令（缺省 = legacy 直跑形态，dev 链兼容入口）。
    #[command(subcommand)]
    pub command: Option<Command>,

    /// 附着模式（仅 `serve` 子命令有效）：已有实例占用管理端口时，核验身份
    /// 并等待其退出，然后接管为新 owner；身份不符或 API 不可达则立即退出。
    /// supervisord autorestart 配合 `exit 0`（正常退出不重启）使用。
    #[arg(long, global = true, env = "APP_CLI_ATTACH")]
    pub attach: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// 常驻 server：探针 + 管理 API + 热部署端点 + 服务编排（容器形态）。
    Serve,

    /// 本地编译工具：逐服务执行 `[build].command`（`--dev` 走三分派）→ artifact
    /// 校验 →（可选）组装产物态部署布局。gen-lock 的伴生命令，与 `serve` 组成
    /// 本地三步闭环（校验 → 编译 → 运行）。不做平台专属（发布 zip/上传/任务
    /// SSE/静态产物路由一致性检查）。
    Build {
        /// dev 三分派：配 `[devbuild]` 执行之；仅配 `[devrun]` 的服务跳过编译
        /// （devrun 自足）；未配 `[devrun]` 回落 `[build].command`（产物落源码目录）。
        #[arg(long)]
        dev: bool,

        /// 产物态部署布局目录（平台 `.run` 的本地等价）：每服务展开 artifact
        /// （zip 解压 / static 拷内容目录）+ 拷入 release.lock.toml；随后
        /// `app-cli --workspace <DIR> serve` 即可产物态运行。要求先跑过 `--gen-lock`。
        #[arg(long, value_name = "DIR")]
        deploy_dir: Option<PathBuf>,

        /// 只构建指定 service_id（逗号分隔）；缺省 = 全部 enabled 服务。
        #[arg(long, value_name = "IDS")]
        only: Option<String>,
    },

    /// 单服务进程包装（supervisord 动态 program 的 command）：读 spec → exec 服务本体。
    RunService {
        /// 部署代（spec 目录段 = /run/app-cli/specs/{RELEASE_ID}/）。
        #[arg(value_name = "RELEASE_ID")]
        release_id: String,
        /// 服务 ID（对应 release.lock.services[].service_id；pingap 用 "pingap"）。
        #[arg(value_name = "SERVICE_ID")]
        service_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    /// B01 后续：全局运行参数两种顺序都合法——子命令后置选项是主流 CLI
    /// 直觉形态（`app-cli serve --workspace X`）；顶层顺序保持兼容
    /// （`app-cli --workspace X serve`，旧脚本/调用方不变）。
    #[test]
    fn global_runtime_flags_accepted_in_both_positions() {
        let after = CliArgs::try_parse_from([
            "app-cli",
            "serve",
            "--workspace",
            "/ws/after",
            "--log-dir",
            "/logs/after",
            "--admin-addr",
            "127.0.0.1:3999",
            "--pingap-bin",
            "/bin/pingap",
            "--attach",
        ])
        .expect("flags after subcommand must parse");
        assert!(matches!(after.command, Some(Command::Serve)));
        assert_eq!(after.workspace, std::path::PathBuf::from("/ws/after"));
        assert_eq!(after.log_dir, std::path::PathBuf::from("/logs/after"));
        assert_eq!(after.admin_addr, "127.0.0.1:3999");
        assert_eq!(after.pingap_bin, std::path::PathBuf::from("/bin/pingap"));
        assert!(after.attach);

        let before = CliArgs::try_parse_from([
            "app-cli",
            "--workspace",
            "/ws/before",
            "--admin-addr",
            "127.0.0.1:3998",
            "serve",
        ])
        .expect("top-level order must stay accepted (legacy callers)");
        assert!(matches!(before.command, Some(Command::Serve)));
        assert_eq!(before.workspace, std::path::PathBuf::from("/ws/before"));

        // 子命令级参数仍在子命令后（build --dev 不受 global 影响）
        let build = CliArgs::try_parse_from(["app-cli", "build", "--dev"]).expect("build flags");
        assert!(matches!(
            build.command,
            Some(Command::Build { dev: true, .. })
        ));
    }
}
