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
    /// Seal a stopped runtime generation using authorization JSON on stdin.
    /// Does not start listeners, databases, or application services.
    SealSource(WorkspaceArgs),
    /// 直接前台编排服务（平台开发链路入口）。
    Run(RunArgs),
    /// 构建 workspace 服务，不启动服务。
    Build(BuildArgs),
    /// 校验 manifest 并生成 release.lock.toml，不启动服务。
    GenLock(WorkspaceArgs),
    /// 执行 supervisord 管理的服务 spec。
    RunService(RunServiceArgs),
}

#[derive(Args, Debug, Clone)]
pub struct WorkspaceArgs {
    /// 包含 workspace.manifest.toml 或 release.lock.toml 的工作区。
    #[arg(long, default_value = "/app/code", env = "APP_CLI_WORKSPACE")]
    pub workspace: PathBuf,
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
