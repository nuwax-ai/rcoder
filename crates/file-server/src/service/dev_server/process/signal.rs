//! 跨平台进程组信号 (unix: kill -pgid; windows: taskkill /T)。

#[cfg(unix)]
use nix::sys::signal::Signal;
#[cfg(unix)]
use nix::unistd::{Pid, getpgid};

// ── unix: 进程组信号 (kill -pgid) ──────────────────────────────────────────
/// 杀进程组 (对齐 nuwax killProcess): 优先 kill(-pid) SIGTERM, 降级 kill(pid)。
/// PID 1 防御统一封装在 [`process_utils`]。
/// 返回是否成功送出信号。
#[cfg(unix)]
pub fn kill_process_group(pid: u32) -> bool {
    let Some(process_pid) = system_pid(pid) else {
        return false;
    };
    process_utils::kill_process_group_with_fallback(process_pid.as_raw() as u32, Signal::SIGTERM)
}

/// 强杀进程组 (SIGKILL 升级): SIGTERM 宽限期后进程仍存活时调用, 优先 kill(-pid) SIGKILL, 降级 kill(pid)。
#[cfg(unix)]
pub fn kill_process_group_force(pid: u32) -> bool {
    let Some(process_pid) = system_pid(pid) else {
        return false;
    };
    process_utils::kill_process_group_with_fallback(process_pid.as_raw() as u32, Signal::SIGKILL)
}

/// 读取进程组 ID，用于 stop 去重：同一 Vite/pnpm 进程树只需 kill 一次。
#[cfg(unix)]
pub fn process_group_id(pid: u32) -> Option<u32> {
    getpgid(Some(system_pid(pid)?))
        .ok()
        .and_then(|pgid| u32::try_from(pgid.as_raw()).ok())
}

/// 进程是否仍在运行 (kill pid 0 探活; 对齐 nuwax isProcessRunning)。
#[cfg(unix)]
pub fn is_process_running(pid: u32) -> bool {
    use nix::sys::signal::kill;
    let Some(process_pid) = system_pid(pid) else {
        return false;
    };
    // kill(pid, None) == 信号 0, 不实际杀, 仅探测
    match kill(process_pid, None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true, // 存在但无权限
        Err(_) => false,
    }
}

/// 将 Tokio 返回的无符号 PID 安全转换为 Unix `pid_t`。
/// PID 0 代表当前进程组，不允许作为外部子进程 PID 使用。
#[cfg(unix)]
fn system_pid(pid: u32) -> Option<Pid> {
    i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .map(Pid::from_raw)
}

// ── windows: 无进程组概念，用 taskkill /T 递归杀进程树 ──────────────────────
// SIGTERM 在 Windows 无直接对应；force=false 走 taskkill /T（尽量优雅），
// force=true 加 /F 强杀整树。
#[cfg(windows)]
pub fn kill_process_group(pid: u32) -> bool {
    kill_tree_windows(pid, false)
}
#[cfg(windows)]
pub fn kill_process_group_force(pid: u32) -> bool {
    kill_tree_windows(pid, true)
}
#[cfg(windows)]
fn kill_tree_windows(pid: u32, force: bool) -> bool {
    let mut cmd = std::process::Command::new("taskkill");
    cmd.arg("/PID").arg(pid.to_string()).arg("/T");
    if force {
        cmd.arg("/F");
    }
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}
/// Windows 无进程组；返回 pid 本身用于 stop 去重（同 pid 只 kill 一次）。
#[cfg(windows)]
pub fn process_group_id(pid: u32) -> Option<u32> {
    Some(pid)
}
/// tasklist 探活：CSV 输出首列含 "{pid}", 即在运行。
#[cfg(windows)]
pub fn is_process_running(pid: u32) -> bool {
    let needle = format!("\"{pid}\",");
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(needle.as_str()))
        .unwrap_or(false)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::system_pid;

    #[test]
    fn system_pid_rejects_values_outside_positive_pid_t_range() {
        assert!(system_pid(0).is_none());
        assert!(system_pid(u32::MAX).is_none());
        assert_eq!(system_pid(1).map(nix::unistd::Pid::as_raw), Some(1));
    }
}
