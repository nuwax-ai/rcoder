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
use crate::proxy::pingap::{build_pingap_config, ProxyEntry, PINGAP_PORT};

/// 编排主入口。
pub async fn run(args: &CliArgs) -> Result<()> {
    // 1. 自动发现子项目 + 组装服务清单
    let specs = manifest::build_specs(&args.workspace)
        .context("discover + build service specs")?;

    // 2. wait PG（PG 由 supervisor [program:postgresql] 托管，秒级就绪；失败不阻断）
    wait_for_pg().await;

    // 3. 各子项目：migrate → start
    let mut children: Vec<(String, Child)> = Vec::new();
    for spec in &specs {
        // migrate（如有）—— 失败不阻断其他项目（多项目隔离），但 error! 暴露 +
        // stdout/stderr 已落 app-cli 日志（run_transient 内打印），便于排障（Fail Fast）。
        if let Some(migrate_cmd) = spec.run.migrate.as_ref().filter(|c| !c.is_empty()) {
            info!("🛠️  migrate {}", spec.name);
            if let Err(e) = run_transient(migrate_cmd, &args.workspace.join(&spec.dir)).await {
                error!("❌ migrate {} failed: {e} — continuing（其他项目仍启动）", spec.name);
            }
        }
        // start
        if spec.run.command.is_empty() {
            warn!("⚠️  {} 无 [run].command，跳过", spec.name);
            continue;
        }
        match start_service(spec, &args.workspace, &args.log_dir) {
            Ok(child) => children.push((spec.name.clone(), child)),
            Err(e) => {
                error!("❌ start {} failed: {e}", spec.name);
                return Err(e);
            }
        }
    }

    // 4. 生成 pingap 配置 + spawn pingap
    let proxy_entries: Vec<ProxyEntry> = specs
        .iter()
        .filter_map(|s| {
            s.proxy.as_ref().map(|p| ProxyEntry {
                name: s.name.clone(),
                port: s.port,
                proxy: p.clone(),
                health: s.health.clone(),
            })
        })
        .collect();
    if let Err(e) = start_pingap(&args.workspace, &proxy_entries, &args.pingap_bin, &mut children).await {
        warn!("⚠️  pingap 启动失败: {e} — 继续仅子项目");
    }

    if children.is_empty() {
        anyhow::bail!("no service started");
    }

    // 5. supervise（阻塞直到任一退出或信号）
    info!("✅ all services started, supervising {} process(es)", children.len());
    supervise(children).await;
    Ok(())
}

// ── PG 等待 ──────────────────────────────────────────────────────────────────

/// pg_isready 轮询（最多 30 次 × 2s = 60s），失败不阻断（PG 可能晚于 app-cli 启）。
async fn wait_for_pg() {
    let host = std::env::var("PGHOST").unwrap_or_else(|_| "localhost".into());
    let port = std::env::var("PGPORT").unwrap_or_else(|_| "5432".into());
    let user = std::env::var("POSTGRES_USER").unwrap_or_else(|_| "app".into());
    let pwd = std::env::var("POSTGRES_PASSWORD").unwrap_or_else(|_| "app".into());

    for i in 1..=30u8 {
        let result = Command::new("pg_isready")
            .arg("-h").arg(&host)
            .arg("-p").arg(&port)
            .arg("-U").arg(&user)
            .env("PGPASSWORD", &pwd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status().await;
        if matches!(result, Ok(s) if s.success()) {
            info!("✅ PG ready (after {i} attempt(s))");
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    warn!("⚠️  PG not ready after 30 attempts — continuing anyway");
}

// ── 子项目启动 ─────────────────────────────────────────────────────────────────

/// 启动一个子项目（[run].command + PORT/HOSTNAME env + stdout/stderr → 轮转日志）。
fn start_service(
    spec: &ServiceSpec,
    ws_root: &Path,
    log_dir: &Path,
) -> Result<Child> {
    let cwd = ws_root.join(&spec.dir);
    let out_path = log_dir.join(format!("{}.out.log", spec.dir));
    let err_path = log_dir.join(format!("{}.err.log", spec.dir));

    let mut cmd = Command::new(&spec.run.command[0]);
    cmd.args(&spec.run.command[1..])
        .current_dir(&cwd)
        .env("HOSTNAME", "0.0.0.0")
        .env("PORT", spec.port.to_string())
        .envs(&spec.env) // 项目级 env（覆盖 workspace 级同名变量）
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()
        .with_context(|| format!("spawn {}: {}", spec.name, spec.run.command.join(" ")))?;

    // pipe → 带轮转的日志文件（append 模式，不 truncate；超 10MB rotate，保留 3 份）
    if let Some(stdout) = child.stdout.take() {
        let p = out_path.clone();
        tokio::spawn(crate::log::writer::pipe_to_rotating_file(stdout, p, None, None));
    }
    if let Some(stderr) = child.stderr.take() {
        let p = err_path.clone();
        tokio::spawn(crate::log::writer::pipe_to_rotating_file(stderr, p, None, None));
    }

    info!("🚀 start {} on :{} (pid={})", spec.name, spec.port,
        child.id().unwrap_or(0));
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

/// 生成 pingap 配置（build_pingap_config）→ 写到 ws_root/pingap/pingap.toml → spawn pingap。
async fn start_pingap(
    ws_root: &Path,
    proxy_entries: &[ProxyEntry],
    pingap_bin: &Path,
    children: &mut Vec<(String, Child)>,
) -> Result<()> {
    let conf = match build_pingap_config(proxy_entries) {
        Some(c) => c,
        None => {
            info!("no [proxy] declared → skip pingap");
            return Ok(());
        }
    };
    let conf_dir = ws_root.join("pingap");
    tokio::fs::create_dir_all(&conf_dir).await?;
    let conf_path = conf_dir.join("pingap.toml");
    tokio::fs::write(&conf_path, &conf).await?;
    info!("📝 pingap config → {}", conf_path.display());

    // spawn pingap（manifest 权威：每次启动从 manifest 重新生成；admin UI 改动临时，重启覆盖）
    let mut cmd = Command::new(pingap_bin);
    cmd.arg("-c").arg(&conf_path).arg("--autoreload");
    // admin UI（从环境变量读 user/pass/addr；默认开 /pingap 路径，共用 :9080 端口）
    if let (Ok(user), Ok(pass)) = (
        std::env::var("PINGAP_ADMIN_USER"),
        std::env::var("PINGAP_ADMIN_PASSWORD"),
    ) {
        let addr = std::env::var("PINGAP_ADMIN_ADDR")
            .unwrap_or_else(|_| format!("0.0.0.0:{}/pingap", PINGAP_PORT));
        let admin = format!("{user}:{pass}@{addr}");
        cmd.arg(format!("--admin={admin}"));
        info!("📡 pingap admin UI → http://{addr}");
    } else {
        info!("ℹ️  PINGAP_ADMIN_USER/PASSWORD 未设 → admin UI 关闭（设置环境变量开启）");
    }
    let child = cmd.spawn().context("spawn pingap")?;
    info!("🚀 start pingap on :{} (pid={})", PINGAP_PORT, child.id().unwrap_or(0));
    children.push(("pingap".into(), child));
    Ok(())
}

// ── supervise（信号 + 任一退出 → kill all → return）─────────────────────────────

/// 优雅停机宽限期（秒）：先 SIGTERM，超时后 SIGKILL。
/// 对齐 agent_runner shutdown 惯例，避免 DB/写文件类子进程丢未刷盘数据。
const SHUTDOWN_GRACE_SECS: u64 = 5;

/// 阻塞直到收到 SIGINT/SIGTERM 或任一子进程退出 → 优雅停止所有子进程 → return。
async fn supervise(mut children: Vec<(String, Child)>) {
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
    shutdown_all(children).await;
}

/// 优雅停止所有子进程：SIGTERM → 等 `SHUTDOWN_GRACE_SECS` → SIGKILL 残留。
async fn shutdown_all(mut children: Vec<(String, Child)>) {
    // 1. SIGTERM 所有子进程
    for (_, child) in children.iter_mut() {
        send_term(child);
    }
    info!(
        "🛑 SIGTERM sent to {} process(es); grace {}s",
        children.len(),
        SHUTDOWN_GRACE_SECS
    );

    // 2. grace 窗口内轮询收尸
    let mut done = vec![false; children.len()];
    let deadline = std::time::Instant::now() + Duration::from_secs(SHUTDOWN_GRACE_SECS);
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
            let _ = child.start_kill();
            warn!("💀 force-killed {name} (SIGKILL after grace)");
        }
    }
}

/// 向子进程发 SIGTERM（unix）；其他平台无 SIGTERM，退化为 SIGKILL。
#[cfg(unix)]
fn send_term(child: &mut Child) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
}

#[cfg(not(unix))]
fn send_term(child: &mut Child) {
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
    use tokio::signal::unix::{signal, SignalKind};
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
