//! Explicit subcommands own their options. Runtime configuration is independent
//! of clap so the orchestration kernel does not depend on CLI dispatch.
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "app-cli",
    version,
    about = "Cross-platform UserApp build and service manager",
    after_help = "Typical workflow:\n  app-cli validate --workspace <WORKSPACE>\n  app-cli gen-lock --workspace <WORKSPACE>\n  app-cli build --workspace <WORKSPACE> --deploy-dir <DEPLOY_DIR>\n  app-cli serve --workspace <DEPLOY_DIR>\n\nPlace options after the subcommand. Use app-cli <COMMAND> --help for details."
)]
pub struct CliArgs {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Start a persistent service manager for start, stop, restart, and deployment operations.
    ///
    /// The manager keeps running and exposes the management API. Use --control-only
    /// to restore management access without automatically starting application services.
    /// This command does not build the workspace.
    Serve(ServeArgs),
    /// Run workspace services in the foreground and supervise them until stopped or exited.
    ///
    /// Starts services from release.lock.toml without building. When managing services
    /// locally, the process stays running after startup. If an owner already exists,
    /// the request is coordinated through that owner. Set APP_CLI_RUN_PROFILE=dev
    /// to prefer [devrun]; otherwise [run] is used. Prefer serve for persistent
    /// management, repeated start/stop operations, and deployments.
    Run(RunArgs),
    /// Build workspace services and optionally assemble a deployment directory without starting services.
    Build(BuildArgs),
    /// Validate manifests and generate release.lock.toml without starting services.
    GenLock(GenLockArgs),
    /// Check source configuration without writing files, building, or starting services.
    ///
    /// Reads workspace and project manifests and compiles the selected proxy profile.
    /// A successful check covers configuration only; it does not prove runtime readiness.
    Validate(ValidateArgs),
    /// Run an individual service (internal entry point used by supervisord).
    RunService(RunServiceArgs),
    /// Query application service readiness without starting or stopping services.
    Readiness(ReadinessArgs),
    /// Query status, recover, stop services, or shut down the CLI through the supervision channel.
    ///
    /// Works independently of the application management API. stop stops application
    /// services while keeping management access; shutdown shuts down the entire CLI;
    /// recover ends the previous execution and restores management access.
    Owner(OwnerArgs),
    /// Resolve deployment journal state conflicts (administrative command).
    Journal {
        #[command(subcommand)]
        command: JournalCommand,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub enum OwnerAction {
    /// Query the supervisor, application process generation, and recovery status.
    Status,
    /// End the previous execution and restore management access.
    Recover,
    /// Stop application services while keeping management access.
    Stop,
    /// Stop application services and shut down the CLI.
    Shutdown,
}

#[derive(Args, Debug, Clone)]
pub struct OwnerArgs {
    /// Action to perform through the supervision channel.
    #[arg(value_enum)]
    pub action: OwnerAction,
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
    /// Reuse this ID when retrying the same operation; a new ID is generated if omitted.
    #[arg(long)]
    pub request_id: Option<String>,
    /// Expected application process generation; a mismatch rejects the operation to protect a newer generation.
    #[arg(long)]
    pub generation: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ReadinessArgs {
    /// Management API address to query; defaults to loopback for queries executed inside a container.
    #[arg(long, default_value = "127.0.0.1:3010", env = "APP_CLI_ADMIN_ADDR")]
    pub admin_addr: String,
    /// Request JSON output (results are always written to stdout as JSON).
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum JournalCommand {
    /// Resolve conflicting state roots by archiving superseded legacy records (auditable and reversible).
    Adopt(JournalAdoptArgs),
}

#[derive(Args, Debug, Clone)]
pub struct JournalAdoptArgs {
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
    /// Archive legacy records without checking whether they are settled or superseded.
    /// The operator asserts that the authoritative state root is the sole source of truth.
    /// Records are renamed with a .superseded-<ts> suffix, not deleted, so the action
    /// can be reversed. The command still refuses to proceed if either lock is held.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone)]
pub struct WorkspaceArgs {
    /// Workspace containing workspace.manifest.toml or release.lock.toml.
    #[arg(long, default_value = "/app/code", env = "APP_CLI_WORKSPACE")]
    pub workspace: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct GenLockArgs {
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
    /// Preview proxy rules for devrun; the lock file retains both dev and prod settings.
    #[arg(long)]
    pub dev: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ValidateArgs {
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
    /// Check the development proxy profile; production manifest requirements still apply.
    #[arg(long)]
    pub dev: bool,
    /// Write one structured configuration report to stdout.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct RuntimeOptions {
    /// Directory for runtime and service logs.
    #[arg(long, default_value = "/app/logs", env = "APP_CLI_LOG_DIR")]
    pub log_dir: PathBuf,
    /// Listen address for the management API.
    #[arg(long, default_value = "0.0.0.0:3010", env = "APP_CLI_ADMIN_ADDR")]
    pub admin_addr: String,
    /// Path to a compatible Pingap executable.
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
    /// Attach to an owner with a matching identity and wait to take over.
    #[arg(long, env = "APP_CLI_ATTACH")]
    pub attach: bool,
    /// Restore management access only and wait for explicit operations; do not automatically start services or deploy from environment variables.
    #[arg(long, conflicts_with = "attach")]
    pub control_only: bool,
}

#[derive(Args, Debug, Clone)]
pub struct BuildArgs {
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
    /// Select development-mode build steps (devbuild/devrun).
    #[arg(long)]
    pub dev: bool,
    /// Assemble build outputs into this deployment directory.
    #[arg(long, value_name = "DIR")]
    pub deploy_dir: Option<PathBuf>,
    /// Build only the specified enabled service IDs (comma-separated).
    #[arg(long, value_name = "IDS")]
    pub only: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct RunServiceArgs {
    /// Release ID for the service specification.
    #[arg(value_name = "RELEASE_ID")]
    pub release_id: String,
    /// Service ID to run from the release.
    #[arg(value_name = "SERVICE_ID")]
    pub service_id: String,
    /// Directory for runtime and service logs.
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
