//! Explicit subcommands own their options. Runtime configuration is independent
//! of clap so the orchestration kernel does not depend on CLI dispatch.
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "app-cli",
    version,
    about = "跨平台 UserApp 构建与服务管理器",
    after_help = "常用流程：\n  app-cli build --workspace <工作区> --deploy-dir <部署目录>\n  app-cli serve --workspace <部署目录>\n\n参数放在子命令后；用 app-cli <子命令> --help 查看详细说明。"
)]
pub struct CliArgs {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// 启动常驻服务管理器，处理启动、停止、重启和部署。
    ///
    /// 服务管理器持续运行并提供管理 API。仅恢复管理入口、不自动启动业务时，
    /// 使用 --control-only。此命令不执行工作区构建。
    Serve(ServeArgs),
    /// 前台运行工作区服务，持续监督到停止或退出。
    ///
    /// 不执行构建；按 release.lock.toml 启动服务。本地编排时进程持续运行，
    /// 不会在服务启动后立即退出；已有所有者时通过所有者协调。
    /// 设置 APP_CLI_RUN_PROFILE=dev 可优先使用 [devrun]，否则使用 [run]。
    /// 需要常驻管理、反复启停和部署时，优先使用 serve。
    Run(RunArgs),
    /// 构建工作区服务，可生成部署目录；不启动服务。
    Build(BuildArgs),
    /// 校验 manifest 并生成 release.lock.toml，不启动服务。
    GenLock(GenLockArgs),
    /// 内部服务执行入口，由 supervisord 调用。
    RunService(RunServiceArgs),
    /// 查询业务服务是否就绪，不启动或停止服务。
    Readiness(ReadinessArgs),
    /// 通过独立监督通道查询、恢复、停止业务或关闭 CLI。
    ///
    /// 不依赖业务管理 API 是否可用。stop 停止业务并保留管理入口，
    /// shutdown 关闭整个 CLI；recover 收束旧执行并恢复管理入口。
    Owner(OwnerArgs),
    /// 处理部署日志的状态冲突（运维命令）。
    Journal {
        #[command(subcommand)]
        command: JournalCommand,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum OwnerAction {
    /// 查询监督进程、业务进程代次与恢复状态。
    Status,
    /// 收束旧执行，恢复管理入口。
    Recover,
    /// 停止业务，保留管理入口。
    Stop,
    /// 停止业务并关闭 CLI。
    Shutdown,
}

#[derive(Args, Debug, Clone)]
pub struct OwnerArgs {
    #[arg(value_enum)]
    pub action: OwnerAction,
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
    /// 重试同一操作时沿用；不提供则生成新的请求 ID。
    #[arg(long)]
    pub request_id: Option<String>,
    /// 期望的业务进程代次；不匹配时拒绝操作，避免影响新代次。
    #[arg(long)]
    pub generation: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ReadinessArgs {
    /// 管理 API 查询地址（exec 场景容器内固定 loopback）。
    #[arg(long, default_value = "127.0.0.1:3010", env = "APP_CLI_ADMIN_ADDR")]
    pub admin_addr: String,
    /// JSON 输出（查询结果恒以 JSON 写 stdout，flag 保留为显式契约）。
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum JournalCommand {
    /// 裁决双权威域冲突：归档被取代的陈旧 legacy 记录（可审计、可回滚）。
    Adopt(JournalAdoptArgs),
}

#[derive(Args, Debug, Clone)]
pub struct JournalAdoptArgs {
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
    /// 跳过 settled/新旧判定归档陈旧 legacy 记录——操作者断言权威状态根为
    /// 唯一真相。归档只改名（.superseded-<ts>）可回滚，不删除；任一侧锁被
    /// 活持有者持有时仍拒绝。
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone)]
pub struct WorkspaceArgs {
    /// 包含 workspace.manifest.toml 或 release.lock.toml 的工作区。
    #[arg(long, default_value = "/app/code", env = "APP_CLI_WORKSPACE")]
    pub workspace: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct GenLockArgs {
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
    /// 预览 devrun 对应的代理规则；锁文件同时保留 dev/prod 配置。
    #[arg(long)]
    pub dev: bool,
}

#[derive(Args, Debug, Clone)]
pub struct RuntimeOptions {
    /// 运行态及服务日志目录。
    #[arg(long, default_value = "/app/logs", env = "APP_CLI_LOG_DIR")]
    pub log_dir: PathBuf,
    /// 管理 API 监听地址。
    #[arg(long, default_value = "0.0.0.0:3010", env = "APP_CLI_ADMIN_ADDR")]
    pub admin_addr: String,
    /// 匹配版本的 Pingap 可执行文件路径。
    #[arg(
        long,
        default_value = "/usr/local/bin/pingap",
        env = "APP_CLI_PINGAP_BIN"
    )]
    pub pingap_bin: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct RunArgs {
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
    #[command(flatten)]
    pub runtime: RuntimeOptions,
}

#[derive(Args, Debug, Clone)]
pub struct ServeArgs {
    #[command(flatten)]
    pub run: RunArgs,
    /// 附着到身份匹配的所有者，等待接管。
    #[arg(long, env = "APP_CLI_ATTACH")]
    pub attach: bool,
    /// 仅恢复管理面，等待显式运行操作；不自动启动业务或执行环境变量部署。
    #[arg(long, conflicts_with = "attach")]
    pub control_only: bool,
}

#[derive(Args, Debug, Clone)]
pub struct BuildArgs {
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
    /// 按开发模式选择构建步骤（devbuild/devrun）。
    #[arg(long)]
    pub dev: bool,
    /// 构建后组装产物部署目录。
    #[arg(long, value_name = "DIR")]
    pub deploy_dir: Option<PathBuf>,
    /// 只构建指定的已启用服务 ID（逗号分隔）。
    #[arg(long, value_name = "IDS")]
    pub only: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct RunServiceArgs {
    #[arg(value_name = "RELEASE_ID")]
    pub release_id: String,
    #[arg(value_name = "SERVICE_ID")]
    pub service_id: String,
    #[arg(long, default_value = "/app/logs", env = "APP_CLI_LOG_DIR")]
    pub log_dir: PathBuf,
}

/// Normalized runtime configuration; never contains CLI actions.
#[derive(Debug, Clone, Default)]
pub struct RuntimeArgs {
    pub workspace: PathBuf,
    pub log_dir: PathBuf,
    pub admin_addr: String,
    pub pingap_bin: PathBuf,
    pub attach: bool,
    pub control_only: bool,
}

impl From<RunArgs> for RuntimeArgs {
    fn from(args: RunArgs) -> Self {
        Self {
            workspace: args.workspace.workspace,
            log_dir: args.runtime.log_dir,
            admin_addr: args.runtime.admin_addr,
            pingap_bin: args.runtime.pingap_bin,
            attach: false,
            control_only: false,
        }
    }
}

impl From<ServeArgs> for RuntimeArgs {
    fn from(args: ServeArgs) -> Self {
        Self {
            attach: args.attach,
            control_only: args.control_only,
            ..args.run.into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_seal_command_is_rejected_while_normal_run_remains_available() {
        assert!(
            CliArgs::try_parse_from(["app-cli", "seal-source", "--workspace", "project"]).is_err()
        );
        assert!(matches!(
            CliArgs::try_parse_from(["app-cli", "run", "--workspace", "project"])
                .unwrap()
                .command,
            Command::Run(_)
        ));
    }

    #[test]
    fn readiness_subcommand_owns_query_options() {
        let cli = CliArgs::try_parse_from([
            "app-cli",
            "readiness",
            "--json",
            "--admin-addr",
            "127.0.0.1:3010",
        ])
        .expect("readiness options");
        let Command::Readiness(args) = cli.command else {
            panic!("expected readiness")
        };
        assert_eq!(args.admin_addr, "127.0.0.1:3010");
        assert!(args.json);
        // 默认 loopback 管理 addr（exec 场景约定）
        let cli = CliArgs::try_parse_from(["app-cli", "readiness"]).expect("defaults");
        let Command::Readiness(args) = cli.command else {
            panic!("expected readiness")
        };
        assert_eq!(args.admin_addr, "127.0.0.1:3010");
        assert!(!args.json);
    }

    #[test]
    fn serve_options_reach_normalized_runtime() {
        let cli = CliArgs::try_parse_from([
            "app-cli",
            "serve",
            "--workspace",
            "project with spaces",
            "--log-dir",
            "logs",
            "--admin-addr",
            "127.0.0.1:3999",
            "--pingap-bin",
            "tools/pingap",
            "--attach",
        ])
        .expect("serve options");
        let Command::Serve(serve) = cli.command else {
            panic!("expected serve")
        };
        let runtime = RuntimeArgs::from(serve);
        assert_eq!(runtime.workspace, PathBuf::from("project with spaces"));
        assert_eq!(runtime.log_dir, PathBuf::from("logs"));
        assert_eq!(runtime.admin_addr, "127.0.0.1:3999");
        assert_eq!(runtime.pingap_bin, PathBuf::from("tools/pingap"));
        assert!(runtime.attach);
    }

    #[test]
    fn control_only_is_explicit_serve_bootstrap_not_attach_or_run() {
        let cli = CliArgs::try_parse_from(["app-cli", "serve", "--control-only"]).unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("serve");
        };
        assert!(RuntimeArgs::from(args).control_only);
        assert!(
            CliArgs::try_parse_from(["app-cli", "serve", "--control-only", "--attach"]).is_err()
        );
        assert!(CliArgs::try_parse_from(["app-cli", "run", "--control-only"]).is_err());
    }

    #[test]
    fn build_owns_workspace_and_build_options() {
        let cli = CliArgs::try_parse_from([
            "app-cli",
            "build",
            "--workspace",
            "project",
            "--dev",
            "--deploy-dir",
            "output",
            "--only",
            "web,api",
        ])
        .expect("build options");
        let Command::Build(build) = cli.command else {
            panic!("expected build")
        };
        assert_eq!(build.workspace.workspace, PathBuf::from("project"));
        assert!(build.dev);
        assert_eq!(build.deploy_dir, Some(PathBuf::from("output")));
        assert_eq!(build.only.as_deref(), Some("web,api"));
    }
}
