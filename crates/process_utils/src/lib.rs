//! Unix 进程信号工具: 带 PID 1 防御的进程组 kill。
//!
//! 背景 (P1-10): 容器内子进程可能拿到 PID 1 (如 docker exec 场景), 此时:
//! - `kill(-1)` 语义是「向**所有**进程组发信号」, 绝不能发;
//! - 内核默认忽略 PID 1 的未注册信号 (含 SIGTERM/SIGKILL)。
//!
//! 本 crate 统一封装该防御, 供 file-server / agent_abstraction / app-cli 复用,
//! 避免各处重复实现产生语义漂移。
//!
//! 注意: 进程组 kill (`kill(-pid)`) 要求目标进程以**进程组组长**启动
//! (如 `Command::process_group(0)`), 此时 pgid == pid; 否则组 kill 会失败,
//! 可用 [`kill_process_group_with_fallback`] 降级到单进程信号。
//!
//! 非 unix 平台无进程组信号语义, 本 crate 不提供任何符号 (调用方自行 cfg 分支)。

#[cfg(unix)]
use nix::sys::signal::{Signal, kill};
#[cfg(unix)]
use nix::unistd::Pid;

#[cfg(unix)]
pub use nix::sys::signal::Signal as KillSignal;

/// 向 pid 所在进程组发信号 (PID 1 防御版)。
///
/// - `pid > 1`: 发 `-pid` (整个进程组, 可清理孙进程);
/// - `pid == 1`: 退化为单进程信号 (kill(-1) 会波及所有进程组, 绝不能发;
///   且 PID 1 信号通常被内核忽略, 调用方应另行依赖 init 收割);
/// - `pid == 0` 或超出 i32 范围: 拒绝发送 (kill(0) 会波及调用者自身进程组)。
///
/// 返回信号是否成功送达。
#[cfg(unix)]
pub fn kill_process_group(pid: u32, signal: Signal) -> bool {
    let Some(target) = safe_signal_target(pid) else {
        return false;
    };
    kill(target, signal).is_ok()
}

/// 向 pid 所在进程组发信号, 组 kill 失败时降级为单进程信号。
///
/// 相比 [`kill_process_group`], 多一层兜底: 进程组不存在/无权限时
/// (如目标不是组长进程、进程组已退出), 仍尝试对进程本身发信号。
/// 返回信号是否成功送达。
#[cfg(unix)]
pub fn kill_process_group_with_fallback(pid: u32, signal: Signal) -> bool {
    let Ok(p) = i32::try_from(pid) else {
        return false;
    };
    if p == 0 {
        // kill(0) 语义是「调用者自身进程组」, 绝不能发
        return false;
    }
    if p == 1 {
        // PID 1: 退化为单进程信号 (见模块文档)
        return kill(Pid::from_raw(p), signal).is_ok();
    }
    match kill(Pid::from_raw(-p), signal) {
        Ok(()) => true,
        Err(_) => kill(Pid::from_raw(p), signal).is_ok(),
    }
}

/// 计算 PID 1 安全的信号目标: `pid > 1` → 进程组 (`-pid`), 否则单进程。
/// `pid == 0` 或超出 i32 范围返回 `None` (调用方应放弃发送)。
#[cfg(unix)]
fn safe_signal_target(pid: u32) -> Option<Pid> {
    let p = i32::try_from(pid).ok()?;
    if p == 0 {
        return None;
    }
    Some(if p > 1 {
        Pid::from_raw(-p)
    } else {
        Pid::from_raw(p)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// spawn 一个 sleep 子进程作为进程组组长, 返回其 pid。
    /// pre_exec 里 setpgid(0,0) 是 std `Command::process_group(0)` 的等价实现
    /// (后者在当前工具链不可用); FFI 调用是这里使用 unsafe 的明确理由。
    fn spawn_sleeper() -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().expect("spawn sleep")
    }

    fn wait_exited(child: &mut std::process::Child) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn kills_process_group_of_real_child() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        assert!(kill_process_group(pid, Signal::SIGTERM));
        assert!(wait_exited(&mut child), "child must exit after SIGTERM");
    }

    #[test]
    fn fallback_variant_also_kills_real_child() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        assert!(kill_process_group_with_fallback(pid, Signal::SIGKILL));
        assert!(wait_exited(&mut child), "child must exit after SIGKILL");
    }

    #[test]
    fn rejects_pid_zero() {
        // kill(0) 会波及调用者自身进程组, 必须拒绝
        assert!(!kill_process_group(0, Signal::SIGTERM));
        assert!(!kill_process_group_with_fallback(0, Signal::SIGTERM));
        assert!(safe_signal_target(0).is_none());
    }

    #[test]
    fn pid_one_degrades_to_single_process_target() {
        // 不发真实信号 (PID 1 不可触碰), 只验证目标计算不取负
        let target = safe_signal_target(1).expect("pid 1 must map to single-process target");
        assert_eq!(target.as_raw(), 1);
    }

    #[test]
    fn group_target_is_negative_pid() {
        let target = safe_signal_target(12345).expect("normal pid");
        assert_eq!(target.as_raw(), -12345);
    }

    #[test]
    fn rejects_pid_over_i32_range() {
        assert!(safe_signal_target(u32::MAX).is_none());
        assert!(!kill_process_group(u32::MAX, Signal::SIGTERM));
        assert!(!kill_process_group_with_fallback(u32::MAX, Signal::SIGTERM));
    }

    #[test]
    fn nonexistent_pid_returns_false() {
        // 取一个几乎不可能存在的 pid (低于 pid_max 上限)
        assert!(!kill_process_group(4_000_000, Signal::SIGTERM));
    }
}
