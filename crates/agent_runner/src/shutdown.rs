//! 进程生命周期管理模块
//!
//! 负责处理 agent_runner 的 panic hook 和优雅关闭（SIGTERM/SIGINT）。
//!
//! ## Panic Hook
//!
//! 当 agent_runner panic 时，将完整的 panic 信息（包括 backtrace）写入日志文件
//! `/app/container-logs/agent_runner_panic.log`，这样即使容器被销毁，
//! 也能通过挂载的日志目录找到崩溃原因。
//!
//! ## 优雅关闭（Unix）
//!
//! agent_runner 在容器内是 PID 1（init），监听 SIGTERM（Docker stop / K8s Pod 删除）
//! 和 SIGINT（Ctrl+C）。收到信号后先有界停止同容器 supervisord（若在，见
//! [`stop_supervisord_bounded`]——否则 PG/dbx/ttyd 被 cgroup SIGKILL，PG 每世
//! crash close），再 `process::exit(0)` 让进程"自愿退出"。
//!
//! 注意：PID 1 受 kernel `SIGNAL_UNKILLABLE` 保护，`kill self SIGKILL` 会被静默丢弃，
//! 故不能用信号自杀，必须用 exit() syscall。详见 `setup_shutdown_handler` 注释。
//! 其余游离子进程（不在 supervisord 树内的）由 cgroup 自动回收（容器化部署，非 JuiceFS）。

use std::fs::OpenOptions;
use std::io::Write;
use std::panic;
use std::path::PathBuf;

#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

// ─── Panic Hook ──────────────────────────────────────────────────────────────

/// 设置自定义 Panic Hook
///
/// 当 agent_runner panic 时，将完整的 panic 信息（包括 backtrace）写入日志文件。
/// 这样即使容器被销毁，也能通过挂载的日志目录找到崩溃原因。
pub fn set_panic_hook() {
    let default_hook = panic::take_hook();

    panic::set_hook(Box::new(move |panic_info| {
        // 立即写入日志文件（不依赖 tracing，确保在 panic 时也能写入）
        if let Err(e) = write_panic_to_file(panic_info) {
            // 如果文件写入失败，尝试输出到 stderr
            eprintln!("❌ [PANIC] Failed to write panic log file: {}", e);
        }

        // 同时输出到 stderr（Docker 会捕获到容器日志）
        eprintln!("═══════════════════════════════════════════════════════════");
        eprintln!("[PANIC] agent_runner encountered a fatal error!");
        eprintln!("═══════════════════════════════════════════════════════════");
        if let Some(location) = panic_info.location() {
            eprintln!(
                "panic.location: {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            );
        }
        eprintln!("panic.payload: {}", panic_info);
        eprintln!("═══════════════════════════════════════════════════════════");

        // 调用默认 hook（会终止进程）
        default_hook(panic_info);
    }));
}

/// 将 panic 信息写入日志文件
fn write_panic_to_file(panic_info: &panic::PanicHookInfo) -> std::io::Result<()> {
    // 日志文件路径：/app/container-logs/agent_runner_panic.log（使用已有的挂载目录）
    let log_path = PathBuf::from("/app/container-logs/agent_runner_panic.log");

    // 确保目录存在
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // 打开文件（追加模式）
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    // 获取当前时间
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");

    // 写入 panic 信息
    writeln!(
        file,
        "═══════════════════════════════════════════════════════════"
    )?;
    writeln!(file, "[PANIC] agent_runner encountered a fatal error!")?;
    writeln!(file, "time: {}", now)?;
    writeln!(
        file,
        "═══════════════════════════════════════════════════════════"
    )?;
    if let Some(location) = panic_info.location() {
        writeln!(
            file,
            "panic.location: {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        )?;
    }
    writeln!(file, "panic.payload: {}", panic_info)?;

    // 写入 backtrace（受 RUST_BACKTRACE 环境变量控制）
    let backtrace = std::backtrace::Backtrace::capture();
    if backtrace.status() == std::backtrace::BacktraceStatus::Captured {
        writeln!(file, "Backtrace:\n{}", backtrace)?;
    }

    writeln!(
        file,
        "═══════════════════════════════════════════════════════════\n"
    )?;

    // 强制刷新到磁盘
    file.flush()?;

    eprintln!("✅ Panic info written to: {}", log_path.display());

    Ok(())
}

// ─── 优雅关闭 ────────────────────────────────────────────────────────────────

/// 向同容器内 supervisord 发送 SIGTERM 并有界等待其退出(若存在)。
///
/// 背景(start-up.sh 布局): agent_runner 经 `exec` 成为 PID 1; supervisord 由
/// start-up.sh 以 daemon 方式启动(孤儿化挂在 PID 1 下、不同会话), PG/dbx/ttyd/
/// app-cli 均在其管理树内。pid1 crate 收到 TERM 只转发给 re-exec 的那一个子进程,
/// 不广播; 若这里直接 exit, 容器销毁时 cgroup 对 supervisord 树一律 SIGKILL ——
/// PG 每世 crash close(下一世 pg.err.log "database system was interrupted"),
/// supervisord 的优先级停止链(app-cli→ttyd→dbx→postgres)从未执行。
///
/// 行为(全部 fail-open, 不阻塞退出):
/// - pidfile 不存在 → 完全 no-op(同一二进制跑无 supervisord 的镜像);
/// - pidfile 内容非法 / pid 已非 supervisord(陈旧文件、pid 复用)→ no-op;
/// - 发 TERM 后轮询等待退出, 上限 [`SUPERVISORD_STOP_BUDGET`](10s, 必须 <
///   pid1 timeout 15s < k8s grace 30s); 超时放行 —— 慢停链不拖住容器退出,
///   保住"删 Pod 周期快"的既有语义(实测脏停止时周期 8-9s, 修复后目标 <15s)。
// Unix-only（nix 信号 + /proc 探活）；与模块内 tokio::signal::unix 的门控
// 形态一致（本模块本就不参与非 Unix 构建）。
#[cfg(unix)]
fn stop_supervisord_bounded() {
    use std::time::{Duration, Instant};

    const PID_FILE: &str = "/var/run/supervisord.pid";
    /// supervisord 停止预算: 必须小于 pid1 crate 的 timeout(15s)与 k8s grace(30s),
    /// 否则修不动"周期快"的语义。10s 覆盖典型停止链(各程序毫秒级退出, PG INT 亚秒)。
    const SUPERVISOR_STOP_BUDGET: Duration = Duration::from_secs(10);
    const POLL_INTERVAL: Duration = Duration::from_millis(100);

    let raw = match std::fs::read_to_string(PID_FILE) {
        Ok(s) => s.trim().to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return, // 无 supervisord 的镜像: no-op
        Err(e) => {
            eprintln!("[shutdown] read {PID_FILE} failed: {e}; skip supervisord stop");
            return;
        }
    };
    let pid: i32 = match raw.parse() {
        Ok(p) if p > 1 => p,
        _ => {
            eprintln!("[shutdown] invalid supervisord pidfile content: {raw:?}; skip");
            return;
        }
    };
    // pid 复用防线: 只对确认是 supervisord 的进程发信号
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(comm) if comm.trim() == "supervisord" => {}
        other => {
            eprintln!("[shutdown] pid {pid} is not supervisord ({other:?}); skip");
            return;
        }
    }
    let target = nix::unistd::Pid::from_raw(pid);
    if let Err(e) = nix::sys::signal::kill(target, Some(nix::sys::signal::Signal::SIGTERM)) {
        eprintln!("[shutdown] failed to signal supervisord {pid}: {e}; continue exit");
        return;
    }
    let started = Instant::now();
    while started.elapsed() < SUPERVISOR_STOP_BUDGET {
        // kill(pid, 0) 探活: 进程退出并被 PID 1 回收后返回 ESRCH
        match nix::sys::signal::kill(target, None) {
            Err(nix::errno::Errno::ESRCH) => {
                eprintln!(
                    "[shutdown] supervisord {pid} exited after {:.1}s (priority stop chain ran)",
                    started.elapsed().as_secs_f32()
                );
                return;
            }
            _ => std::thread::sleep(POLL_INTERVAL),
        }
    }
    eprintln!(
        "[shutdown] supervisord {pid} still running after {}s budget; exit anyway (fast cycle preserved)",
        SUPERVISOR_STOP_BUDGET.as_secs()
    );
}

/// 设置优雅关闭信号处理器
///
/// 监听 SIGTERM（Docker stop / K8s Pod 删除）和 SIGINT（Ctrl+C），收到后先有界
/// 停止同容器 supervisord（若在，见 [`stop_supervisord_bounded`]），再
/// `process::exit(0)` 退出。详见函数内注释关于 PID 1 SIGNAL_UNKILLABLE 的说明。
pub fn setup_shutdown_handler() {
    // 独立线程 + 独立 tokio runtime 监听 SIGTERM/SIGINT。
    // 实测 (p3e): 独立线程的 tokio::signal recv 能可靠收到 SIGTERM
    //   (tokio signal driver 的 signalfd 绑定到首次初始化它的 runtime, 这里是独立 runtime)。
    // 而 main runtime 的 tokio::spawn recv 反而收不到 (p6-main 实测)。
    //
    // 收到信号 → process::exit(0): 进程"自愿退出"。
    //   ⚠️ 为什么不用 kill self SIGKILL: 容器内 agent_runner 是 PID 1, 受 kernel
    //      SIGNAL_UNKILLABLE 保护 (每个 PID namespace 的 init 都有此标志)。
    //      SIGKILL 无 handler → 对 PID 1 静默丢弃 (kill -9 1 实测无效)。
    //      而 exit() syscall 属于"自愿退出", 不属于信号投递, 不受此保护 → 可正常终止。
    //   参考: man 2 kill; kernel/signal.c SIGNAL_UNKILLABLE。
    //   K8s 删 pod 先发 SIGTERM, 我们在这里立即 exit → 远早于 grace 15s, 不触发兜底等待。
    //   若 handler 失效, K8s grace 后由 CRI 从父 namespace 发 SIGKILL (force, 绕过保护) 兜底。
    // 容器 (PID 1): cgroup 自动回收子进程; 现 CephFS/emptyDir (非 JuiceFS), 无 FUSE 句柄问题。
    std::thread::Builder::new()
        .name("shutdown-signal".to_string())
        .spawn(|| {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("[shutdown] Failed to build signal runtime: {}", e);
                    return;
                }
            };
            rt.block_on(async move {
                let mut sigterm = match signal(SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[shutdown] Failed to register SIGTERM: {}", e);
                        return;
                    }
                };
                let mut sigint = match signal(SignalKind::interrupt()) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[shutdown] Failed to register SIGINT: {}", e);
                        return;
                    }
                };
                tokio::select! {
                    _ = sigterm.recv() => {
                        eprintln!("[shutdown] Received SIGTERM, exiting process (voluntary exit, bypasses PID 1 SIGNAL_UNKILLABLE)");
                    }
                    _ = sigint.recv() => {
                        eprintln!("[shutdown] Received SIGINT, exiting process (voluntary exit, bypasses PID 1 SIGNAL_UNKILLABLE)");
                    }
                }
                // ⚠️ PID 1 (容器 init) 受 kernel SIGNAL_UNKILLABLE 保护:
                //    kill self SIGKILL / kill -9 1 被静默丢弃 (SIGKILL 无 handler, 实测 agent_runner 未退出)。
                //    但 process::exit(0) 是自愿 exit() syscall, 不是信号, 不受此保护 → 可立即终止。
                // exit 前先有界停掉同容器的 supervisord(若在): PG/dbx/ttyd/app-cli 在其管理树内,
                // 直接 exit 会让 cgroup SIGKILL 它们 —— PG 每世 crash close(下一世 pg.err.log
                // "database system was interrupted")。详见 stop_supervisord_bounded。
                stop_supervisord_bounded();
                // 实测 (docker PID 1): process::exit 收到 SIGTERM 后 0.117s 退出, 无 atexit/flush 挂起。
                // process::exit → libc::exit → atexit + stdio flush + _exit; 对 PID 1 同样有效。
                std::process::exit(0);
            });
        })
        .expect("failed to spawn shutdown-signal thread");
}
