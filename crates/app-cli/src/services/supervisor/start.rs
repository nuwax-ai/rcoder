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

/// 运行一个临时命令（migrate），等它结束后返回。
///
/// 捕获 stdout/stderr（不 `Stdio::null()` 丢弃）：成功走 `info!`，失败带 stderr 返回错误，
/// 便于排障（Fail Fast：暴露而非吞掉）。
pub(crate) async fn run_migration_with_receipt(
    spec: &ServiceSpec,
    release: &manifest::ReleaseLock,
    workspace: &Path,
    pg: Option<&shared_types::StartPgCredential>,
) -> Result<()> {
    run_migration_with_receipt_cancel(spec, release, workspace, pg, None).await
}

pub(crate) async fn run_migration_with_receipt_cancel(
    spec: &ServiceSpec,
    release: &manifest::ReleaseLock,
    workspace: &Path,
    pg: Option<&shared_types::StartPgCredential>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    anyhow::ensure!(
        !cancel.is_some_and(|token| token.is_cancelled()),
        "Migration cancelled before dispatch"
    );
    let environment = service_environment(spec, pg)?;
    let directory = workspace.join(&spec.dir);
    validate_transient_input(&spec.run.migrate, &directory)?;
    let identity = crate::migration_journal::identity(release, &spec.service_id)?;
    let Some(receipt) = crate::migration_journal::MigrationJournal::begin(workspace, identity)?
    else {
        info!(service = %spec.service_id, "Migration already confirmed for this artifact");
        return Ok(());
    };
    if let Some(cancel) = cancel {
        run_transient_cancellable(
            &spec.run.migrate,
            &directory,
            &environment,
            Duration::from_secs(300),
            Some(cancel),
        )
        .await?;
    } else {
        run_transient_with_env(&spec.run.migrate, &directory, &environment).await?;
    }
    receipt
        .complete()
        .context("persist confirmed database migration")
}

pub(crate) async fn run_transient_with_env(
    argv: &[String],
    cwd: &Path,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    run_transient_with_environment_and_timeout(argv, cwd, env, Duration::from_secs(300)).await
}

#[cfg(all(test, unix))]
pub(super) async fn run_transient_with_timeout(
    argv: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<()> {
    run_transient_with_environment_and_timeout(argv, cwd, &Default::default(), timeout).await
}

pub(super) async fn run_transient_with_environment_and_timeout(
    argv: &[String],
    cwd: &Path,
    env: &std::collections::BTreeMap<String, String>,
    timeout: Duration,
) -> Result<()> {
    run_transient_cancellable(argv, cwd, env, timeout, None).await
}

/// Reject local input failures before creating an uncertain migration receipt.
/// This does not prove spawn will succeed; failures after dispatch remain fenced.
pub(super) fn validate_transient_input(argv: &[String], cwd: &Path) -> Result<()> {
    let program = argv.first().context("migration command is empty")?;
    anyhow::ensure!(!program.trim().is_empty(), "migration executable is empty");
    anyhow::ensure!(
        argv.iter().all(|argument| !argument.contains('\0')),
        "migration command contains a NUL byte"
    );
    let metadata = std::fs::metadata(cwd)
        .with_context(|| format!("inspect migration working directory {}", cwd.display()))?;
    anyhow::ensure!(
        metadata.is_dir(),
        "migration working directory is not a directory"
    );
    Ok(())
}

pub(super) async fn run_transient_cancellable(
    argv: &[String],
    cwd: &Path,
    env: &std::collections::BTreeMap<String, String>,
    timeout: Duration,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<()> {
    validate_transient_input(argv, cwd)?;
    let program = argv.first().context("migration command is empty")?;
    // R01：migrate 同样走受管进程树——shell 包装的迁移命令 spawn 的子孙
    // （工作进程/DB writer）整树归属，超时与收尾均收束确认。
    let mut cmd = Command::new(crate::win_cmd::resolve_spawn_program(program));
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_owned(cmd, None)
        .await
        .with_context(|| format!("spawn migrate: {}", argv.join(" ")))?;

    // 并发 drain stdout/stderr：防 pipe 被写满阻塞 + 捕获失败原因
    let stdout = child.take_stdout();
    let stderr = child.take_stderr();
    let out_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_string(&mut buf).await;
        }
        buf
    });
    let err_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut s) = stderr {
            let _ = s.read_to_string(&mut buf).await;
        }
        buf
    });

    // 只等根进程退出（root 语义）：root 正常退出后仍可能有后台孙进程在写
    // （迁移 shell `cmd &` 后台化），超时与收尾判定不能被树级 wait 阻塞。
    let outcome = tokio::select! {
        result = tokio::time::timeout(timeout, child.wait_root()) => Some(result),
        () = async {
            match cancel {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        } => None,
    };
    // A shell may exit successfully while descendants still execute migrations.
    // Confirm the original tree is stopped (and gone) before allowing recovery
    // or success — stop(ZERO) 对已退出的 root 直通，对残留孙进程 TERM→KILL
    // 并整树收束确认。
    let stop_outcome = child.stop(Duration::ZERO).await;
    if stop_outcome == StopOutcome::Unconfirmed {
        out_task.abort();
        err_task.abort();
        return Err(ShutdownUnconfirmed(
            "Migration process tree shutdown was not confirmed".into(),
        )
        .into());
    }
    let status = match outcome {
        Some(Ok(status)) => status.context("wait migrate")?,
        None => {
            out_task.abort();
            err_task.abort();
            anyhow::bail!("Migration cancelled; database outcome requires reconciliation");
        }
        Some(Err(_)) => {
            out_task.abort();
            err_task.abort();
            anyhow::bail!(
                "migrate timed out after {}ms: {}",
                timeout.as_millis(),
                argv.join(" ")
            );
        }
    };
    let out = out_task.await.unwrap_or_default();
    let err = err_task.await.unwrap_or_default();

    if !out.trim().is_empty() {
        info!("migrate stdout:\n{out}");
    }
    if !status.success() {
        if !err.trim().is_empty() {
            warn!("migrate stderr:\n{err}");
        }
        anyhow::bail!("migrate exited {status}");
    }
    Ok(())
}
