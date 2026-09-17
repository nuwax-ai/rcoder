//! 跨平台进程树管理（cross-platform.md §4）。
//!
//! 保证：受管进程的所有后代都随停止操作退出，不遗留孤儿进程。
//!
//! 平台实现：
//! - Linux/macOS: `process_group(0)` 创建独立进程组 → `kill_process_group`
//! - Windows: Job Object → `TerminateJobObject` 清理所有成员
//!
//! 统一接口 [`ManagedChild`] 封装 spawn + 停止 + 等待 + 确认。

use anyhow::{Context, Result};
use std::process::ExitStatus;
use std::time::Duration;

/// 受管子进程（持有平台特定的进程树管理资源）。
pub(crate) struct ManagedChild {
    inner: tokio::process::Child,
    #[cfg(windows)]
    _job: windows_job::JobGuard,
}

/// 停止结果（cross-platform.md §4：区分正常收尾、强制停止、清理未确认）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    /// 进程在宽限期内正常退出。
    Graceful(ExitStatus),
    /// 宽限期后强制终止（SIGKILL / TerminateJobObject）。
    Forced,
    /// 无法确认进程退出（日志句柄、锁文件残留等）。
    Unconfirmed,
}

/// 启动受管子进程（自动归属进程树管理资源）。
///
/// spawn 后立即完成平台特定归属，避免逃逸窗口。
pub(crate) fn spawn_managed(cmd: &mut tokio::process::Command) -> Result<ManagedChild> {
    // Unix：使用 process_group(0) 创建独立进程组（子进程自成组长）
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let child = cmd
        .spawn()
        .context("spawn managed child process")?;

    #[cfg(windows)]
    {
        let pid = child.id().context("child has no PID")?;
        let job = windows_job::JobGuard::new(pid)?;
        return Ok(ManagedChild { inner: child, _job: job });
    }

    #[cfg(not(windows))]
    Ok(ManagedChild { inner: child })
}

impl ManagedChild {
    /// 进程 ID（诊断用）。
    pub fn id(&self) -> Option<u32> {
        self.inner.id()
    }

    /// 优雅停止：发送停止信号 → 等待宽限期 → 强制终止。
    ///
    /// `grace_period` 为业务进程的正常退出等待时间。
    pub async fn stop(&mut self, grace_period: Duration) -> StopOutcome {
        // 第一步：发送优雅停止信号
        self.send_stop_signal();

        // 等待宽限期
        match tokio::time::timeout(grace_period, self.inner.wait()).await {
            Ok(Ok(status)) => return StopOutcome::Graceful(status),
            Ok(Err(_)) => return StopOutcome::Unconfirmed,
            Err(_) => {} // 超时，继续强制终止
        }

        // 宽限期后强制终止
        self.force_kill();

        match tokio::time::timeout(Duration::from_secs(5), self.inner.wait()).await {
            Ok(Ok(_status)) => StopOutcome::Forced,
            Ok(Err(_)) => StopOutcome::Unconfirmed,
            Err(_) => StopOutcome::Unconfirmed,
        }
    }

    /// 发送优雅停止信号（平台特定）。
    fn send_stop_signal(&mut self) {
        #[cfg(unix)]
        {
            if let Some(pid) = self.inner.id() {
                process_utils::kill_process_group(pid, process_utils::KillSignal::SIGTERM);
            }
        }
        // Windows：Ctrl+C 不可靠（需要 CREATE_NEW_PROCESS_GROUP + 控制台进程）。
        // Job Object 的 TerminateJobObject 是 Windows 上停止进程树的可靠手段。
        // 此处为 no-op，由 force_kill 保证停止。
        #[cfg(windows)]
        {
            let _ = self.inner.id();
        }
    }

    /// 强制终止（平台特定）。
    fn force_kill(&mut self) {
        #[cfg(unix)]
        {
            if let Some(pid) = self.inner.id() {
                process_utils::kill_process_group(pid, process_utils::KillSignal::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        {
            // tokio 的 start_kill 在 Windows 上调用 TerminateProcess（只杀直接子进程）
            // 但 Job Object 会清理所有成员
            self.inner.start_kill().ok();
        }
    }

    /// 等待进程退出。
    pub async fn wait(&mut self) -> Result<ExitStatus> {
        self.inner.wait().await.context("wait for managed child")
    }
}

// ── Windows Job Object 实现 ──

#[cfg(windows)]
mod windows_job {
    use anyhow::Result;

    /// Job Object RAII 守卫：进程归入 Job → JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    /// 确保 owner 退出时清理所有成员。
    pub(crate) struct JobGuard {
        handle: windows_sys::Win32::Foundation::HANDLE,
    }

    impl JobGuard {
        pub fn new(pid: u32) -> Result<Self> {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
            };

            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                anyhow::bail!(
                    "CreateJobObjectW failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            // 设置 JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE：句柄关闭时自动终止所有成员
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let result = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as _,
                    std::mem::size_of_val(&limits) as u32,
                )
            };
            if result == 0 {
                unsafe { CloseHandle(handle) };
                anyhow::bail!(
                    "SetInformationJobObject failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            // 打开进程并归入 Job
            let proc = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
            if proc.is_null() {
                unsafe { CloseHandle(handle) };
                anyhow::bail!(
                    "OpenProcess({pid}) failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            let ok = unsafe { AssignProcessToJobObject(handle, proc) };
            unsafe { CloseHandle(proc) };
            if ok == 0 {
                unsafe { CloseHandle(handle) };
                anyhow::bail!(
                    "AssignProcessToJobObject failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            Ok(Self { handle })
        }
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(self.handle, 1);
                windows_sys::Win32::Foundation::CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 受管进程的后代随父进程组停止而退出。
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_process_group_stop() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 60 & sleep 60 & wait"]);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = spawn_managed(&mut cmd).unwrap();
        let pid = child.id().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let outcome = child.stop(Duration::from_secs(1)).await;
        assert!(
            matches!(outcome, StopOutcome::Graceful(_) | StopOutcome::Forced),
            "process group must be stopped, got: {outcome:?}"
        );

        // 确认进程组中的所有进程都已退出
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!process_utils::process_group_exists(pid).unwrap_or(true),
            "process group {pid} should not exist after stop");
    }

    /// 等待受管进程正常退出。
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_normal_exit() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"]);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = spawn_managed(&mut cmd).unwrap();
        let status = child.wait().await.unwrap();
        assert!(status.success());
    }

    /// 强制终止：宽限期后进程被终止
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_force_kill_after_timeout() {
        let mut cmd = tokio::process::Command::new("python3");
        cmd.args(["-c", "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"]);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = spawn_managed(&mut cmd).unwrap();

        let outcome = child.stop(Duration::from_millis(100)).await;
        assert!(
            matches!(outcome, StopOutcome::Graceful(_) | StopOutcome::Forced),
            "process must be stopped, got: {outcome:?}"
        );
    }

    /// 进程组在 stop 后不存在（SIGTERM 路径）
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_process_group_terminated_by_sigterm() {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", "sleep 60"]);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = spawn_managed(&mut cmd).unwrap();
        let pid = child.id().unwrap();

        let outcome = child.stop(Duration::from_secs(2)).await;
        assert!(
            matches!(outcome, StopOutcome::Graceful(_) | StopOutcome::Forced),
            "process must be stopped, got: {outcome:?}"
        );

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !process_utils::process_group_exists(pid).unwrap_or(true),
            "process group {pid} should not exist after stop"
        );
    }

    /// Windows：受管进程能正常 spawn 和退出
    #[cfg(windows)]
    #[tokio::test]
    async fn managed_child_windows_spawn_and_exit() {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.args(["/C", "exit 0"]);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = spawn_managed(&mut cmd).unwrap();
        let status = child.wait().await.unwrap();
        assert!(status.success());
    }

    /// Windows：受管进程能被强制停止
    #[cfg(windows)]
    #[tokio::test]
    async fn managed_child_windows_force_stop() {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.args(["/C", "ping -n 60 127.0.0.1 >nul"]);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = spawn_managed(&mut cmd).unwrap();

        let outcome = child.stop(Duration::from_millis(500)).await;
        assert!(
            matches!(outcome, StopOutcome::Graceful(_) | StopOutcome::Forced),
            "Windows process must be stopped, got: {outcome:?}"
        );
    }
}
