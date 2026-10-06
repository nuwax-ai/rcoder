use super::*;

// ── 子项目启动 ─────────────────────────────────────────────────────────────────

/// 启动一个子项目（[run].command + PORT/HOSTNAME env + stdout/stderr → 轮转日志）。
/// dev 形态编排信号（源码态 dev 链路）：平台 dev server spawn 本进程时注入
/// `APP_CLI_RUN_PROFILE=dev`。仅影响启动命令选择（[devrun] 优先、[run] 兜底），
/// 端口注入/pingap/健康检查/拓扑与生产编排完全一致；未注入（生产 serve、
/// 本地直跑）恒走 [run]——与既有行为逐字节一致。
pub fn dev_run_profile() -> bool {
    std::env::var("APP_CLI_RUN_PROFILE").as_deref() == Ok("dev")
}

/// 服务的生效启动命令：dev 形态且配置了 [devrun] 时用 devrun.command（热加载，
/// 跑源码），否则 [run].command。（[devbuild] 的回落在平台侧 dev 链路执行，
/// app-cli 不消费该字段。）
pub(crate) fn effective_run_argv(spec: &ServiceSpec, dev_profile: bool) -> &[String] {
    if dev_profile && let Some(devrun) = &spec.devrun {
        return &devrun.command;
    }
    &spec.run.command
}

pub(super) async fn start_service(
    spec: &ServiceSpec,
    argv: &[String],
    ws_root: &Path,
    log_dir: &Path,
    release_id: &str,
    pg: Option<&shared_types::StartPgCredential>,
) -> Result<ManagedChild> {
    let cwd = ws_root.join(&spec.dir);
    let service_log_dir = log_dir.join(&spec.service_id);
    tokio::fs::create_dir_all(&service_log_dir)
        .await
        .with_context(|| format!("create service log dir {}", service_log_dir.display()))?;
    let out_path = service_log_dir.join("runtime.out.log");
    let err_path = service_log_dir.join("runtime.err.log");

    // R01：真实业务进程统一受管进程树 spawn（Unix 进程组 / Windows Job
    // Object）——npm/pnpm shell 启动的 node 子孙全部归属，停止收束整树。
    let mut cmd = Command::new(crate::win_cmd::resolve_spawn_program(&argv[0]));
    cmd.args(&argv[1..])
        .current_dir(&cwd)
        .envs(service_environment(spec, pg)?)
        // Runtime-owned variables are applied last so even a hand-crafted
        // release lock cannot override service identity, paths, or ports.
        .env("HOSTNAME", "0.0.0.0")
        .env("PORT", spec.port.to_string())
        .env("APP_LOG_DIR", &service_log_dir)
        .env("APP_SERVICE_ID", &spec.service_id)
        .env("APP_RELEASE_ID", release_id);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = spawn_owned(cmd, None)
        .await
        .with_context(|| format!("spawn {}: {}", spec.service_id, argv.join(" ")))?;

    // pipe → 带轮转的日志文件（append 模式，不 truncate；超 10MB rotate，保留 3 份）
    if let Some(stdout) = child.take_stdout() {
        let p = out_path.clone();
        tokio::spawn(crate::log::writer::pipe_to_rotating_file(
            stdout, p, None, None,
        ));
    }
    if let Some(stderr) = child.take_stderr() {
        let p = err_path.clone();
        tokio::spawn(crate::log::writer::pipe_to_rotating_file(
            stderr, p, None, None,
        ));
    }

    info!(
        "🚀 start {} ({}) on :{} (pid={})",
        spec.service_id,
        spec.name,
        spec.port,
        child.id().unwrap_or(0)
    );
    Ok(child)
}
