//! Explicit subcommands own their options. Runtime configuration is independent
//! of clap so the orchestration kernel does not depend on CLI dispatch.
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(name = "app-cli", version, about = "UserApp 构建与运行管理器")]
pub struct CliArgs {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// 启动常驻运行态所有者及管理 API。
    Serve(ServeArgs),
    /// 直接前台编排服务（平台开发链路入口）。
    Run(RunArgs),
    /// 构建 workspace 服务，不启动服务。
    Build(BuildArgs),
    /// 校验 manifest 并生成 release.lock.toml，不启动服务。
    GenLock(GenLockArgs),
    /// 执行 supervisord 管理的服务 spec。
    RunService(RunServiceArgs),
    /// 纯只读业务就绪查询（GET 管理 API；不进入 serve/run、不启动 owner）。
    Readiness(ReadinessArgs),
    /// 部署 journal 运维（双权威域裁决等）。
    Journal {
        #[command(subcommand)]
        command: JournalCommand,
    },
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
}

impl From<RunArgs> for RuntimeArgs {
    fn from(args: RunArgs) -> Self {
        Self {
            workspace: args.workspace.workspace,
            log_dir: args.runtime.log_dir,
            admin_addr: args.runtime.admin_addr,
            pingap_bin: args.runtime.pingap_bin,
            attach: false,
        }
    }
}

impl From<ServeArgs> for RuntimeArgs {
    fn from(args: ServeArgs) -> Self {
        Self {
            attach: args.attach,
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
