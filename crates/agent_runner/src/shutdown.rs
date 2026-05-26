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
//! 监听 SIGTERM（Docker stop / K8s Pod 删除）和 SIGINT（Ctrl+C），执行以下流程：
//! 1. 从 /proc 构建进程树，递归收集所有后代进程 + 孤儿进程（ppid=1）
//! 2. 按叶子优先顺序发送 SIGTERM，等待最多 3 秒
//! 3. 对仍存活的进程发送 SIGKILL
//! 4. 等待 1 秒让文件缓冲 flush（JuiceFS FUSE 卷）
//! 5. process::exit(0)

use std::fs::OpenOptions;
use std::io::Write;
use std::panic;
use std::path::PathBuf;

#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

use tracing::{debug, error, info, warn};

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
        eprintln!("❌ [PANIC] agent_runner encountered a fatal error!");
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
    writeln!(file, "❌ [PANIC] agent_runner encountered a fatal error!")?;
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

/// 设置优雅关闭信号处理器
///
/// 监听系统信号，实现优雅关闭：
/// - Unix: SIGTERM (Docker stop) + SIGINT (Ctrl+C)
/// - Windows: Ctrl+C
///
/// 关闭流程（Unix）：
/// 1. 从 /proc 构建进程树，递归收集所有后代进程 + 孤儿进程（ppid=1）
/// 2. 按叶子优先顺序发送 SIGTERM，等待最多 3 秒
/// 3. 对仍存活的进程发送 SIGKILL
/// 4. 等待 1 秒让文件缓冲 flush（JuiceFS FUSE 卷）
/// 5. process::exit(0)
pub fn setup_shutdown_handler() -> tokio::task::JoinHandle<()> {
    #[cfg(unix)]
    {
        tokio::spawn(async move {
            // 监听 SIGTERM（Docker stop / K8s Pod 删除）
            let mut sigterm = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "Failed to register SIGTERM handler");
                    return;
                }
            };

            // 监听 SIGINT（Ctrl+C）
            let mut sigint = match signal(SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "Failed to register SIGINT handler");
                    return;
                }
            };

            tokio::select! {
                _ = sigterm.recv() => {
                    info!(signal = "SIGTERM", "Received shutdown signal, starting graceful shutdown");
                    write_shutdown_log("SIGTERM");
                }
                _ = sigint.recv() => {
                    info!(signal = "SIGINT", "Received shutdown signal, starting graceful shutdown");
                    write_shutdown_log("SIGINT");
                }
            }

            // ── Step 1: 清理后代进程和孤儿进程（kill + wait）──
            //
            // 作为容器的 PID 1，必须负责回收整个进程树。
            // 如果不清理，后代进程会继续持有 JuiceFS FUSE 卷上的文件句柄，
            // 导致 kubelet 卸载 FUSE 卷时 unmount 挂起。
            info!("Terminating descendant and orphan processes before shutdown");
            terminate_children();

            // ── Step 2: 等待文件系统缓冲 flush ──
            //
            // 给 Rust 运行时和 glibc 的 atexit handlers 一点时间，
            // 确保 JuiceFS FUSE 卷上的写入 buffer 被持久化到磁盘。
            // process::exit(0) 会跳过所有 Drop 析构函数，所以这里
            // 显式等待 1 秒让后台 flush 线程完成。
            info!("Syncing filesystem buffers before exit (1s delay)");
            std::thread::sleep(std::time::Duration::from_secs(1));

            info!("Graceful shutdown completed, exiting");
            std::process::exit(0);
        })
    }

    #[cfg(not(unix))]
    {
        tokio::spawn(async move {
            // Windows: 仅监听 Ctrl+C
            if let Ok(()) = tokio::signal::ctrl_c().await {
                info!(signal = "Ctrl+C", "Received shutdown signal, starting graceful shutdown");
                write_shutdown_log("Ctrl+C");
            }

            info!("Graceful shutdown completed, exiting");
            std::process::exit(0);
        })
    }
}

/// 终止所有后代进程和孤儿进程（Unix 平台）
///
/// 作为容器的 PID 1，agent_runner 负责清理整个进程树：
///
/// ## 收集范围
///
/// 1. **所有后代进程**: 从 /proc 构建进程树，递归遍历 my_pid 下的全部子孙
///    （不仅直接子进程，还包括子进程的子进程，以此类推）
/// 2. **孤儿进程**: ppid=1 但不在进程树中的进程。
///    这些进程原本是 agent_runner 的后代，因父进程退出被内核 reparent 到 PID 1。
///    它们可能仍持有 JuiceFS FUSE 卷的文件句柄，必须一并清理。
///
/// ## 信号发送顺序（叶子优先）
///
/// 按 **叶子 → 根** 的顺序发信号（reverse post-order），原因：
/// - 先杀叶子进程，避免其父进程被杀后叶子变孤儿、继续持有 FUSE 文件句柄
/// - 父进程在子进程之后收到信号，有机会做自己的清理工作
///
/// ## 设计说明
///
/// 不使用 `kill(0, SIGTERM)`（进程组信号），
/// 因为 PID 1 调用进程组信号会向自身发送 SIGTERM，导致信号处理器重入。
/// 使用 /proc 枚举 + 精确发信号可以避免这个问题。
#[cfg(unix)]
fn terminate_children() {
    use std::time::Duration;

    let my_pid = std::process::id();

    // ── 构建进程树 ──
    //
    // 从 /proc 枚举所有进程，构建:
    //   children_map: ppid → Vec<pid>（每个父进程的子进程列表）
    // 其中 children_map[1] 包含所有 ppid=1 的进程（用于孤儿检测）
    let children_map: std::collections::HashMap<u32, Vec<u32>> = {
        let mut cm: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();

        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let pid: u32 = match entry.file_name().to_string_lossy().parse() {
                    Ok(p) if p > 1 => p,
                    _ => continue,
                };
                if let Some(ppid) = read_ppid(pid) {
                    cm.entry(ppid).or_default().push(pid);
                }
            }
        }
        cm
    };

    // ── 收集所有后代进程（叶子优先顺序）──
    //
    // 遍历 my_pid 的直接子节点，递归收集它们的后代。
    // 不直接对 my_pid 调用 collect_descendants，避免把 agent_runner 自身加入列表。
    let mut descendants = Vec::new();
    if let Some(direct_children) = children_map.get(&my_pid) {
        for &child in direct_children {
            collect_descendants(child, &children_map, &mut descendants);
        }
    }

    // ── 捕获孤儿进程（ppid=1，不在已有进程树中）──
    //
    // 容器内 agent_runner 是 PID 1，子进程退出后其子进程被 reparent 到 PID 1。
    // children_map[1] 包含所有 ppid=1 的进程（含 agent_runner 的直接子进程）。
    // 过滤掉已在 descendants 中的进程，剩余的即为孤儿进程。
    // 它们可能仍持有 FUSE 卷的文件句柄，必须一并清理。
    let known: std::collections::HashSet<u32> = descendants.iter().copied().collect();
    let orphans: Vec<u32> = children_map
        .get(&1)
        .map(|children| {
            children
                .iter()
                .copied()
                .filter(|pid| *pid != my_pid && !known.contains(pid))
                .collect()
        })
        .unwrap_or_default();

    if !orphans.is_empty() {
        info!(
            orphan_count = orphans.len(),
            orphan_pids = ?orphans,
            "Found orphaned processes (ppid=1, reparented)"
        );
        descendants.extend(&orphans);
    }

    if descendants.is_empty() {
        debug!("No descendant or orphan processes to terminate");
        return;
    }

    info!(
        process_count = descendants.len(),
        process_pids = ?descendants,
        "Found descendant/orphan processes, sending SIGTERM (leaf-first)"
    );

    // ── Phase 1: SIGTERM（优雅终止，叶子优先）──
    for &pid in &descendants {
        send_signal_safe(pid, nix::sys::signal::Signal::SIGTERM);
    }

    // ── Phase 2: 等待最多 3 秒，每秒检查是否全部退出 ──
    for elapsed in 1..=3 {
        std::thread::sleep(Duration::from_secs(1));
        let alive: Vec<u32> = descendants
            .iter()
            .copied()
            .filter(|&p| is_process_alive(p))
            .collect();
        if alive.is_empty() {
            info!(elapsed_seconds = elapsed, "All descendant processes exited gracefully");
            return;
        }
        warn!(
            alive_count = alive.len(),
            alive_pids = ?alive,
            elapsed_seconds = elapsed,
            "Processes still alive, continuing to wait"
        );
    }

    // ── Phase 3: SIGKILL（强制终止，叶子优先）──
    warn!("Processes did not exit within 3s, sending SIGKILL");
    for &pid in &descendants {
        if is_process_alive(pid) {
            send_signal_safe(pid, nix::sys::signal::Signal::SIGKILL);
        }
    }
    // 给内核一点时间完成 SIGKILL 处理
    std::thread::sleep(Duration::from_millis(500));
}

/// 读取进程的父进程 PID（通过 /proc/{pid}/status 的 PPid 字段）
#[cfg(unix)]
fn read_ppid(pid: u32) -> Option<u32> {
    let status_path = format!("/proc/{}/status", pid);
    let content = std::fs::read_to_string(&status_path).ok()?;
    for line in content.lines() {
        if let Some(ppid_str) = line.strip_prefix("PPid:") {
            return ppid_str.trim().parse().ok();
        }
    }
    None
}

/// 递归收集进程树中某个节点的所有后代（叶子优先 / reverse post-order）
///
/// 先递归进入子节点，再收集当前节点自身，确保叶子进程排在父进程之前。
/// 这样发送信号时先杀叶子、再杀父进程，避免产生新的孤儿进程。
///
/// 示例：进程树 `agent_runner(1) → bash(10) → node(20) → worker(30)`
/// 收集顺序: `[30, 20, 10]`（叶子优先）
#[cfg(unix)]
fn collect_descendants(
    pid: u32,
    children_map: &std::collections::HashMap<u32, Vec<u32>>,
    result: &mut Vec<u32>,
) {
    if let Some(children) = children_map.get(&pid) {
        for &child in children {
            collect_descendants(child, children_map, result);
        }
    }
    // 每个被传入的 pid 都会被收集（包括当前节点自身）。
    // 调用方（terminate_children）只对 my_pid 的直接子节点调用此函数，
    // 因此 agent_runner 自身（my_pid）永远不会出现在结果列表中。
    result.push(pid);
}

/// 检查进程是否存活（通过检查 /proc/{pid} 目录是否存在）
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    std::fs::metadata(format!("/proc/{}", pid)).is_ok()
}

/// 向进程发送信号（忽略 ESRCH 错误，进程可能已退出）
#[cfg(unix)]
fn send_signal_safe(pid: u32, sig: nix::sys::signal::Signal) {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    if pid > 1 {
        match kill(Pid::from_raw(pid as i32), sig) {
            Ok(()) => {
                debug!(pid = pid, signal = ?sig, "Sent signal to child process");
            }
            Err(nix::errno::Errno::ESRCH) => {
                // 进程已退出，正常
                debug!(pid = pid, "Process already exited, signal not sent");
            }
            Err(e) => {
                warn!(
                    pid = pid,
                    signal = ?sig,
                    error = %e,
                    "Failed to send signal to child process"
                );
            }
        }
    }
}

/// 将关闭事件写入日志文件
fn write_shutdown_log(signal: &str) {
    let log_path = PathBuf::from("/app/container-logs/agent_runner_shutdown.log");

    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
        let _ = writeln!(
            file,
            "═══════════════════════════════════════════════════════════"
        );
        let _ = writeln!(
            file,
            "📨 [SHUTDOWN] agent_runner received a shutdown signal"
        );
        let _ = writeln!(file, "signal: {}", signal);
        let _ = writeln!(file, "time: {}", now);
        let _ = writeln!(
            file,
            "═══════════════════════════════════════════════════════════\n"
        );
        let _ = file.flush();
        eprintln!("✅ Shutdown info written to: {}", log_path.display());
    }
}
