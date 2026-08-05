//! 子项目 + pingap 编排核心：wait PG → migrate → start 子项目 → spawn pingap → supervise。
//!
//! 替代 workspace start.sh。由 main.rs 调用 `run(&args)`，前台阻塞直到任一子进程退出或收到信号，
//! 然后 kill 所有子进程 + return → supervisor [program:app] 感知退出 → 整组重启。

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tracing::{error, info, warn};

use crate::config::CliArgs;
use crate::manifest::{self, ServiceSpec};
use crate::proxy::compiler::compile_and_validate;
use crate::proxy::pingap::PINGAP_PORT;
use crate::runtime_status::RuntimeStatusService;

/// 编排主入口。
pub async fn run(args: &CliArgs, runtime_status: RuntimeStatusService) -> Result<()> {
    runtime_status.set_ready(false);
    // 1. 自动发现子项目 + 组装服务清单
    let release = manifest::read_release_lock(&args.workspace).context("load release lock")?;
    validate_runtime_compatibility(&release)?;
    let specs = release.services.clone();

    // 2. wait PG（PG 由 supervisor [program:postgresql] 托管，秒级就绪；失败不阻断）
    wait_for_pg().await?;

    // 3. 各子项目：migrate → start
    let mut children: Vec<(String, Child)> = Vec::new();
    for spec in &specs {
        // migrate（如有）—— 失败不阻断其他项目（多项目隔离），但 error! 暴露 +
        // stdout/stderr 已落 app-cli 日志（run_transient 内打印），便于排障（Fail Fast）。
        if !spec.run.migrate.is_empty() {
            info!("🛠️  migrate {}", spec.name);
            if let Err(e) = run_transient(&spec.run.migrate, &args.workspace.join(&spec.dir)).await
            {
                error!("❌ migrate {} failed: {e}", spec.name);
                return Err(e);
            }
        }
        // start
        if spec.run.command.is_empty() {
            warn!("⚠️  {} 无 [run].command，跳过", spec.name);
            continue;
        }
        match start_service(spec, &args.workspace, &args.log_dir, &release.release_id) {
            Ok(child) => children.push((spec.name.clone(), child)),
            Err(e) => {
                error!("❌ start {} failed: {e}", spec.name);
                return Err(e);
            }
        }
    }

    // 4. 编译、完整验证并启动 Pingap；代理失败时 workspace 不得进入 ready。
    start_pingap(&args.workspace, &args.pingap_bin, &release, &mut children).await?;

    // 5. readiness —— 默认不强依赖后端 app(用户核心诉求:后端有 bug 起不来时容器仍 ready、可排查)。
    //   - 无 [health].bridge_service:app-cli 自给自足,初始化完成即 ready。
    //   - 有 bridge_service:只等那一个后端的 readiness_path;超时 → 保持 NotReady(摘流)
    //     但不 bail/崩溃(liveness /health 仍 200,容器活着,用户可 exec 进去排查)。
    let ready = match &release.bridge_service {
        None => true,
        Some(bridge_id) => match specs.iter().find(|s| &s.service_id == bridge_id) {
            None => {
                warn!(
                    "⚠️  [health].bridge_service '{bridge_id}' not in release services; \
                     defaulting to ready"
                );
                true
            }
            Some(spec) => match wait_for_service_ready(spec).await {
                Ok(()) => {
                    info!("✅ bridge service '{bridge_id}' ready");
                    true
                }
                Err(e) => {
                    warn!(
                        "⏳ bridge service '{bridge_id}' not ready: {e}; \
                         staying NotReady (traffic withheld, liveness unaffected)"
                    );
                    false
                }
            },
        },
    };
    runtime_status.set_ready(ready);

    if children.is_empty() {
        anyhow::bail!("no service started");
    }

    // 5. supervise（阻塞直到任一退出或信号）
    info!(
        "✅ all services started, supervising {} process(es)",
        children.len()
    );
    let shutdown_timeout = specs
        .iter()
        .map(|service| service.run.shutdown_timeout_seconds)
        .max()
        .unwrap_or(30);
    supervise(children, shutdown_timeout).await;
    runtime_status.set_ready(false);
    Ok(())
}

fn validate_runtime_compatibility(release: &workspace_manifest::ReleaseLock) -> Result<()> {
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .context("parse current app-cli version")?;
    let minimum = semver::Version::parse(&release.minimum_app_cli_version)
        .context("parse minimum app-cli version from release lock")?;
    if current < minimum {
        anyhow::bail!("release requires app-cli >= {minimum}, current version is {current}");
    }
    // 运行时身份(pingap 版本/commit、镜像 digest)仅记录日志,不做硬校验:
    // pingap 向前兼容(新版接受旧配置),且 app-runtime 镜像与 pingap 会随升级变动,
    // 硬性相等比对会阻塞容器启动 → 无法平滑升级。此处打印 release.lock 与运行时实际值
    // 供日志追溯;mismatch / 缺失仅 warn,不阻断启动。
    for (name, locked) in [
        ("RCODER_PINGAP_VERSION", release.pingap.version.as_str()),
        ("RCODER_PINGAP_COMMIT", release.pingap.commit.as_str()),
        (
            "RCODER_RUNTIME_IMAGE_DIGEST",
            release.runtime_image_digest.as_str(),
        ),
    ] {
        match std::env::var(name) {
            Ok(runtime) => {
                if runtime == locked {
                    info!("{name}: release={locked} runtime={runtime} (matched)");
                } else {
                    warn!("{name} mismatch (non-fatal, will not block startup): release={locked}, runtime={runtime}");
                }
            }
            Err(_) => warn!(
                "{name} not set in runtime (non-fatal): release={locked}"
            ),
        }
    }
    Ok(())
}

// ── PG 等待 ──────────────────────────────────────────────────────────────────

/// pg_isready 轮询（最多 30 次 × 2s = 60s），失败不阻断（PG 可能晚于 app-cli 启）。
async fn wait_for_pg() -> Result<()> {
    let host = std::env::var("PGHOST").unwrap_or_else(|_| "localhost".into());
    let port = std::env::var("PGPORT").unwrap_or_else(|_| "5432".into());
    let user = std::env::var("POSTGRES_USER").unwrap_or_else(|_| "app".into());
    let pwd = std::env::var("POSTGRES_PASSWORD").unwrap_or_else(|_| "app".into());

    for i in 1..=30u8 {
        let result = Command::new("pg_isready")
            .arg("-h")
            .arg(&host)
            .arg("-p")
            .arg(&port)
            .arg("-U")
            .arg(&user)
            .env("PGPASSWORD", &pwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        if matches!(result, Ok(s) if s.success()) {
            info!("✅ PG ready (after {i} attempt(s))");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    anyhow::bail!("PostgreSQL not ready after 60 seconds");
}

/// 轮询单个桥接后端的 readiness_path 直至就绪(120s 超时)。
///
/// 仅在 workspace.manifest `[health].bridge_service` 显式配置时调用(只等那一个后端)。
/// 默认(不配 bridge)不调本函数 —— app-cli 自给 /ready,不强依赖任何后端。
async fn wait_for_service_ready(spec: &ServiceSpec) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .context("build readiness HTTP client")?;
    let url = format!(
        "http://127.0.0.1:{}{}",
        spec.port, spec.health.readiness_path
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let ready = client
            .get(&url)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        if ready {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "service '{}' readiness timed out after 120 seconds (path {})",
                spec.service_id,
                spec.health.readiness_path
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ── 子项目启动 ─────────────────────────────────────────────────────────────────

/// 启动一个子项目（[run].command + PORT/HOSTNAME env + stdout/stderr → 轮转日志）。
fn start_service(
    spec: &ServiceSpec,
    ws_root: &Path,
    log_dir: &Path,
    release_id: &str,
) -> Result<Child> {
    let cwd = ws_root.join(&spec.dir);
    let service_log_dir = log_dir.join(&spec.service_id);
    std::fs::create_dir_all(&service_log_dir)
        .with_context(|| format!("create service log dir {}", service_log_dir.display()))?;
    let out_path = service_log_dir.join("runtime.out.log");
    let err_path = service_log_dir.join("runtime.err.log");

    let mut cmd = process_group_command(&spec.run.command[0]);
    cmd.args(&spec.run.command[1..])
        .current_dir(&cwd)
        .envs(&spec.env)
        // Runtime-owned variables are applied last so even a hand-crafted
        // release lock cannot override service identity, paths, or ports.
        .env("HOSTNAME", "0.0.0.0")
        .env("PORT", spec.port.to_string())
        .env("APP_LOG_DIR", &service_log_dir)
        .env("APP_SERVICE_ID", &spec.service_id)
        .env("APP_RELEASE_ID", release_id)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}: {}", spec.name, spec.run.command.join(" ")))?;

    // pipe → 带轮转的日志文件（append 模式，不 truncate；超 10MB rotate，保留 3 份）
    if let Some(stdout) = child.stdout.take() {
        let p = out_path.clone();
        tokio::spawn(crate::log::writer::pipe_to_rotating_file(
            stdout, p, None, None,
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        let p = err_path.clone();
        tokio::spawn(crate::log::writer::pipe_to_rotating_file(
            stderr, p, None, None,
        ));
    }

    info!(
        "🚀 start {} on :{} (pid={})",
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
async fn run_transient(argv: &[String], cwd: &Path) -> Result<()> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn migrate: {}", argv.join(" ")))?;

    // 并发 drain stdout/stderr：防 pipe 被写满阻塞 + 捕获失败原因
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
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

    let status = child.wait().await.context("wait migrate")?;
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

// ── pingap 启动 ─────────────────────────────────────────────────────────────────

/// 编译用户权威配置到只读运行目录，`pingap -t` 成功后再启动。
async fn start_pingap(
    ws_root: &Path,
    pingap_bin: &Path,
    release: &workspace_manifest::ReleaseLock,
    children: &mut Vec<(String, Child)>,
) -> Result<()> {
    let runtime_root = std::env::var_os("APP_CLI_PINGAP_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/run/app-cli/pingap".into());
    let conf_path = compile_and_validate(ws_root, &runtime_root, pingap_bin, release).await?;
    info!("📝 effective pingap config → {}", conf_path.display());

    let mut cmd = process_group_command(pingap_bin);
    cmd.arg("-c").arg(&conf_path).arg("--autoreload");
    // Admin deliberately stays disabled: workspace TOML is the sole authority.
    let child = cmd.spawn().context("spawn pingap")?;
    info!(
        "🚀 start pingap on :{} (pid={})",
        PINGAP_PORT,
        child.id().unwrap_or(0)
    );
    children.push(("pingap".into(), child));
    Ok(())
}

// ── supervise（信号 + 任一退出 → kill all → return）─────────────────────────────

/// 优雅停机宽限期（秒）：先 SIGTERM，超时后 SIGKILL。
/// 对齐 agent_runner shutdown 惯例，避免 DB/写文件类子进程丢未刷盘数据。
/// 阻塞直到收到 SIGINT/SIGTERM 或任一子进程退出 → 优雅停止所有子进程 → return。
async fn supervise(mut children: Vec<(String, Child)>, shutdown_timeout_seconds: u64) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("📡 received SIGINT, shutting down");
        }
        _ = wait_sigterm() => {
            info!("📡 received SIGTERM, shutting down");
        }
        exited = poll_any_exit(&mut children) => {
            if let Some(name) = exited {
                error!("❌ {name} exited — shutting down (supervisor will restart)");
            }
        }
    }
    shutdown_all(children, shutdown_timeout_seconds).await;
}

/// 优雅停止所有子进程：SIGTERM → 等 `SHUTDOWN_GRACE_SECS` → SIGKILL 残留。
async fn shutdown_all(mut children: Vec<(String, Child)>, shutdown_timeout_seconds: u64) {
    // 1. SIGTERM 所有子进程
    for (_, child) in children.iter_mut() {
        send_term(child);
    }
    info!(
        "🛑 SIGTERM sent to {} process(es); grace {}s",
        children.len(),
        shutdown_timeout_seconds
    );

    // 2. grace 窗口内轮询收尸
    let mut done = vec![false; children.len()];
    let deadline = std::time::Instant::now() + Duration::from_secs(shutdown_timeout_seconds);
    while std::time::Instant::now() < deadline {
        for (i, (_, child)) in children.iter_mut().enumerate() {
            if !done[i] && matches!(child.try_wait(), Ok(Some(_))) {
                done[i] = true;
            }
        }
        if done.iter().all(|&d| d) {
            info!("✅ all children exited gracefully");
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 3. 超时 SIGKILL 残留
    for (i, (name, child)) in children.iter_mut().enumerate() {
        if !done[i] {
            send_kill(child);
            warn!("💀 force-killed {name} (SIGKILL after grace)");
        }
    }
}

/// 向子进程发 SIGTERM（unix）；其他平台无 SIGTERM，退化为 SIGKILL。
#[cfg(unix)]
fn send_term(child: &mut Child) {
    if let Some(pid) = child.id()
        && !process_utils::kill_process_group(pid, process_utils::KillSignal::SIGTERM)
    {
        // 信号未送达通常意味进程已退出; 仅 debug 留痕 (PID1 防御见 process_utils)
        tracing::debug!(pid, "send_term: signal not delivered");
    }
}

#[cfg(not(unix))]
fn send_term(child: &mut Child) {
    let _ = child.start_kill();
}

#[cfg(unix)]
fn send_kill(child: &mut Child) {
    if let Some(pid) = child.id()
        && !process_utils::kill_process_group(pid, process_utils::KillSignal::SIGKILL)
    {
        tracing::debug!(pid, "send_kill: signal not delivered");
    }
}

#[cfg(not(unix))]
fn send_kill(child: &mut Child) {
    let _ = child.start_kill();
}

/// 轮询所有子进程，第一个退出的返回其名字（500ms 间隔）。
async fn poll_any_exit(children: &mut [(String, Child)]) -> Option<String> {
    loop {
        for (name, child) in children.iter_mut() {
            match child.try_wait() {
                Ok(Some(_)) => return Some(name.clone()),
                Ok(None) => {}
                Err(_) => return Some(name.clone()),
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// 等 SIGTERM（Unix 专属）。handler 安装失败时降级（不 panic），
/// 由 [`tokio::signal::ctrl_c`] / `poll_any_exit` 兜底触发关闭。
#[cfg(unix)]
async fn wait_sigterm() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sig = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            warn!("install SIGTERM handler failed: {e} — SIGTERM 不会被捕获");
            return;
        }
    };
    sig.recv().await;
}

#[cfg(not(unix))]
async fn wait_sigterm() {
    std::future::pending::<()>().await;
}

#[cfg(unix)]
fn process_group_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new("setsid");
    command.arg("--").arg(program);
    command
}

#[cfg(not(unix))]
fn process_group_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    Command::new(program)
}
