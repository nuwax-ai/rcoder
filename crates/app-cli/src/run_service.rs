//! `app-cli run-service {release_id} {service_id}`：supervisord 动态 program 的
//! 服务包装进程。
//!
//! 读 spec → 组装 env（继承 supervisord 环境 + spec.env + runtime 覆盖）→
//! chdir → `exec` 服务本体（unix 下替换进程映像——无包装层残留、信号直达、
//! supervisord 的 stopasgroup 整组管理与 builtin 引擎的进程组语义一致）。
//! spec 缺失/argv 空 = 配置错误：stderr 报因 + 非零退出（supervisord 记
//! FATAL，10 次后放弃——由 server 的部署状态上报暴露）。

use anyhow::Result;

use crate::config::CliArgs;
use crate::svc_spec::ServiceSpecFile;

/// run-service 入口：成功 = 进程映像被服务本体替换（永不返回）；
/// Err = 启动失败（main 转非零退出码）。
pub fn run(release_id: &str, service_id: &str, args: &CliArgs) -> Result<()> {
    let spec = ServiceSpecFile::load(release_id, service_id)?;
    exec_spec(&spec, &args.log_dir)
}

/// 组装并 exec（独立函数便于单测 env 组装逻辑）。
fn exec_spec(spec: &ServiceSpecFile, log_dir: &std::path::Path) -> Result<()> {
    let mut command = std::process::Command::new(&spec.argv[0]);
    command
        .args(&spec.argv[1..])
        .current_dir(&spec.cwd)
        .envs(&spec.env)
        .envs(spec.runtime_env_overrides(log_dir));
    if let Some(port) = spec.port {
        tracing::info!(
            "run-service: exec {} (service={}, port={port})",
            spec.argv.join(" "),
            spec.service_id
        );
    } else {
        tracing::info!(
            "run-service: exec {} (service={})",
            spec.argv.join(" "),
            spec.service_id
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = command.exec(); // 成功不返回
        Err(anyhow::anyhow!(
            "exec {} failed: {err}",
            spec.argv.join(" ")
        ))
    }
    #[cfg(not(unix))]
    {
        let status = command.status().context("spawn service")?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // exec_spec 在 unix 下成功即替换进程——不可直接测成功路径；
    // 组装逻辑经 runtime_env_overrides 测试覆盖（svc_spec），这里只测
    // spec 缺失时的守卫。
    #[test]
    fn missing_spec_is_error() {
        let _guard = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("APP_CLI_SPEC_DIR", dir.path()) };
        let args = CliArgs {
            command: None,
            ..Default::default()
        };
        let err = run("rel-none", "svc-none", &args).unwrap_err();
        assert!(format!("{err:#}").contains("read service spec"));
    }

    #[test]
    fn spec_shape_minimal_fields() {
        let spec = ServiceSpecFile {
            release_id: "r".into(),
            service_id: "s".into(),
            cwd: "/x".into(),
            argv: vec!["sleep".into(), "1".into()],
            env: BTreeMap::new(),
            port: None,
        };
        // 无端口服务（如 pingap）——overrides 不含 PORT
        let overrides = spec.runtime_env_overrides(std::path::Path::new("/logs"));
        assert!(!overrides.contains_key("PORT"));
    }
}
