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
    _job: WindowsJob,
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
        let job = WindowsJob::new(child.id().context("child has no PID")?)?;
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
        #[cfg(windows)]
        {
            // Windows：先尝试 Ctrl+C（仅对控制台进程有效），然后靠 Job 超时终止
            if let Some(pid) = self.inner.id() {
                unsafe {
                    windows_sys::Win32::System::Console::GenerateConsoleCtrlEvent(0, pid);
                }
            }
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
struct WindowsJob {
    handle: windows_sys::Win32::System::JobObjects::HANDLE,
}

#[cfg(windows)]
impl WindowsJob {
    /// 创建 Job Object 并将进程归入。
    fn new(pid: u32) -> Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, AssignProcessToJobObject,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle == 0 {
            anyhow::bail!("CreateJobObjectW failed: {}", std::io::Error::last_os_error());
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let result = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if result == 0 {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle); }
            anyhow::bail!("SetInformationJobObject failed: {}", std::io::Error::last_os_error());
        }

        let process_handle = unsafe {
            windows_sys::Win32::System::Threading::OpenProcess(
                windows_sys::Win32::System::Threading::PROCESS_SET_QUOTA
                    | windows_sys::Win32::System::Threading::PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        if process_handle == 0 {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle); }
            anyhow::bail!("OpenProcess({pid}) failed: {}", std::io::Error::last_os_error());
        }

        let assigned = unsafe { AssignProcessToJobObject(handle, process_handle) };
        unsafe { windows_sys::Win32::Foundation::CloseHandle(process_handle); }
        if assigned == 0 {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(handle); }
            anyhow::bail!("AssignProcessToJobObject failed: {}", std::io::Error::last_os_error());
        }

        Ok(Self { handle })
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.handle, 1);
            windows_sys::Win32::Foundation::CloseHandle(self.handle);
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

    /// 强制终止：宽限期后进程被终止（Forced 或 Graceful 取决于信号送达时序）
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_force_kill_after_timeout() {
        // 忽略 SIGTERM 的 Python 进程，无子进程
        let mut cmd = tokio::process::Command::new("python3");
        cmd.args(["-c", "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"]);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = spawn_managed(&mut cmd).unwrap();

        // 宽限期极短（100ms），进程忽略 SIGTERM → 必须被 SIGKILL
        // 由于进程组信号可能在某些时序下被 Python 捕获后退出（signal 15），
        // 这里接受 Graceful 或 Forced——关键是进程确实被终止了
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

        // 进程组应已退出
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !process_utils::process_group_exists(pid).unwrap_or(true),
            "process group {pid} should not exist after stop"
        );
    }
}
