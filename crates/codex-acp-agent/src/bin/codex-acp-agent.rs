use anyhow::Result;
use clap::Parser;
use codex_arg0::arg0_dispatch_or_else;
use codex_common::CliConfigOverrides;

/// Codex ACP Agent - An ACP-compatible coding agent powered by Codex
/// 
/// 这是 rcoder 项目的定制版本（fork from zed-industries/codex-acp），
/// 支持通过 -c 参数动态覆盖配置，无需修改 ~/.codex/config.toml
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Override a configuration value that would otherwise be loaded from
    /// `~/.codex/config.toml`. Use a dotted path (`foo.bar.baz`) to override
    /// nested values. The `value` portion is parsed as JSON. If it fails to
    /// parse as JSON, the raw string is used as a literal.
    ///
    /// Examples:
    ///   - `-c model="GLM-4.6"`
    ///   - `-c model_provider=glm`
    ///   - `-c model_providers.glm.base_url=https://open.bigmodel.cn/api/coding/paas/v4`
    ///   - `-c 'sandbox_permissions=["disk-full-read-access"]'`
    ///   - `-c shell_environment_policy.inherit=all`
    #[arg(
        short = 'c',
        long = "config",
        value_name = "key=value",
        action = clap::ArgAction::Append,
        global = true,
    )]
    raw_overrides: Vec<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cli_config_overrides = CliConfigOverrides {
        raw_overrides: args.raw_overrides,
    };

    arg0_dispatch_or_else(|codex_linux_sandbox_exe| async move {
        codex_acp::run_main(codex_linux_sandbox_exe, cli_config_overrides).await?;
        Ok(())
    })
}

