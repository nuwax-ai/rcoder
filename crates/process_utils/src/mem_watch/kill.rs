//! 进程终止：用 `pidfd`（rustix 的安全封装，免疫 PID 复用）。
//!
//! 项目红线禁止 `unsafe`，所以这里不用 `libc::syscall` 裸调；
//! `rustix::process::pidfd_open` / `pidfd_send_signal` 是安全封装。

use std::time::Duration;

#[cfg(target_os = "linux")]
mod imp {
    use super::Duration;
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};

    pub(super) async fn terminate(pid: u32, grace: Duration) {
        let Some(target) = Pid::from_raw(pid as i32) else {
            return;
        };
        let Ok(handle) = pidfd_open(target, PidfdFlags::empty()) else {
            // 进程已退出，或内核不支持 pidfd
            return;
        };

        // pidfd 绑定的是"那个进程"，即使 pid 被复用也不会误杀。
        let _ = pidfd_send_signal(&handle, Signal::TERM);
        tokio::time::sleep(grace).await;
        let _ = pidfd_send_signal(&handle, Signal::KILL);
        // handle 是 OwnedFd，离开作用域自动 close
    }
}

/// SIGTERM → `grace` → SIGKILL。
///
/// 非 Linux 平台为空操作（看门狗只在 Linux 容器内运行）。
pub async fn terminate(pid: u32, grace: Duration) {
    #[cfg(target_os = "linux")]
    {
        imp::terminate(pid, grace).await;
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, grace);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminates_a_real_child() {
        let mut child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 60")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("pid");

        terminate(pid, Duration::from_millis(200)).await;

        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("child should exit")
            .expect("wait ok");
        assert!(!status.success());
    }

    #[tokio::test]
    async fn missing_process_is_not_an_error() {
        // 一个几乎不可能存在的 pid：不应 panic，也不应阻塞
        terminate(4_000_000, Duration::from_millis(10)).await;
    }
}
