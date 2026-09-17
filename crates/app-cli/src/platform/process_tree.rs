//! 跨平台进程树管理（cross-platform.md §4）。
//!
//! 保证：受管进程的所有后代都随停止操作退出，不遗留孤儿进程。
//!
//! 平台实现：
//! - Linux/macOS: `process_group(0)` 创建独立进程组 → `kill_process_group`
//! - Windows: `CREATE_SUSPENDED` 挂起 spawn → Job Object 归属 → ResumeThread
//!   （进程开始执行前完成归属，消除逃逸窗口——cross-platform.md §4）
//!
//! 统一接口 [`ManagedChild`] 封装 spawn + 停止 + 等待 + 确认。

use anyhow::{Context, Result};
use std::process::ExitStatus;
use std::time::Duration;

/// 受管子进程（持有平台特定的进程树管理资源）。
pub(crate) struct ManagedChild {
    inner: tokio::process::Child,
    #[cfg(windows)]
    job: windows_job::JobGuard,
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
/// Unix：`process_group(0)` 使子进程自成组长，spawn 原子完成归属。
/// Windows：`CREATE_SUSPENDED` 挂起 spawn → Job 归属 → Resume 主线程，
/// 进程开始执行前已完成 Job 归属（无逃逸窗口）。
pub(crate) fn spawn_managed(cmd: &mut tokio::process::Command) -> Result<ManagedChild> {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        cmd.creation_flags(CREATE_SUSPENDED);
    }

    let mut child = cmd
        .spawn()
        .context("spawn managed child process")?;

    #[cfg(windows)]
    {
        let pid = child.id().context("suspended child has no PID")?;
        // 挂起状态下完成 Job 归属；任一步失败都以 kill 收尾挂起进程
        // （挂起进程无副作用，kill 即彻底回收，不留孤儿）。
        let attach = windows_job::JobGuard::attach_suspended(pid);
        if let Err(error) = &attach {
            tracing::error!("job attach failed for suspended pid {pid}: {error:#}");
            let _ = child.start_kill();
        }
        let job = attach?;
        if let Err(error) = windows_job::resume_main_thread(pid) {
            tracing::error!("resume main thread failed for pid {pid}: {error:#}");
            // 挂起进程无法恢复：Job 兜底终止后如实上抛
            job.terminate_all();
            let _ = child.start_kill();
            anyhow::bail!("resume managed child {pid}: {error:#}");
        }
        Ok(ManagedChild { inner: child, job })
    }

    #[cfg(not(windows))]
    Ok(ManagedChild { inner: child })
}

impl ManagedChild {
    /// 进程 ID（诊断用）。
    pub fn id(&self) -> Option<u32> {
        self.inner.id()
    }

    /// 优雅停止：发送停止信号 → 等待宽限期 → 强制终止进程树。
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

        // 宽限期后强制终止整个进程树
        self.force_kill();

        match tokio::time::timeout(Duration::from_secs(5), self.inner.wait()).await {
            Ok(Ok(_status)) => StopOutcome::Forced,
            Ok(Err(_)) => StopOutcome::Unconfirmed,
            Err(_) => StopOutcome::Unconfirmed,
        }
    }

    /// 发送优雅停止信号（平台特定）。
    #[cfg(unix)]
    fn send_stop_signal(&mut self) {
        if let Some(pid) = self.inner.id() {
            process_utils::kill_process_group(pid, process_utils::KillSignal::SIGTERM);
        }
    }

    /// Windows：无可靠跨树优雅信号（GenerateConsoleCtrlEvent 需
    /// CREATE_NEW_PROCESS_GROUP + 控制台进程）。宽限期内自然退出计
    /// Graceful；超时由 force_kill 的 TerminateJobObject 收束全树。
    #[cfg(windows)]
    fn send_stop_signal(&mut self) {}

    /// 强制终止（平台特定）。
    #[cfg(unix)]
    fn force_kill(&mut self) {
        if let Some(pid) = self.inner.id() {
            process_utils::kill_process_group(pid, process_utils::KillSignal::SIGKILL);
        }
    }

    /// Windows：TerminateJobObject 终止 Job 内全部成员（孙进程一并收束）。
    /// start_kill 仅作用于直接子进程，不作进程树手段。
    #[cfg(windows)]
    fn force_kill(&mut self) {
        self.job.terminate_all();
    }

    /// 等待进程退出。
    pub async fn wait(&mut self) -> Result<ExitStatus> {
        self.inner.wait().await.context("wait for managed child")
    }
}

// ── Windows Job Object 实现 ──

#[cfg(windows)]
mod windows_job {
    use anyhow::{Context, Result};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

    /// Job Object RAII 守卫：`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 确保
    /// 守卫句柄关闭（含 owner 进程崩溃）时自动终止所有成员。
    pub(crate) struct JobGuard {
        handle: HANDLE,
    }

    impl JobGuard {
        /// 创建配置好的 Job 并归入指定进程（进程须处于挂起状态）。
        pub fn attach_suspended(pid: u32) -> Result<Self> {
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
            // 错误路径统一关句柄（KILL_ON_JOB_CLOSE 顺带清理已归入成员）
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as _,
                    std::mem::size_of_val(&limits) as u32,
                )
            };
            if ok == 0 {
                let error = std::io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                anyhow::bail!("SetInformationJobObject failed: {error}");
            }

            let proc = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
            if proc.is_null() {
                let error = std::io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                anyhow::bail!("OpenProcess({pid}) failed: {error}");
            }
            let ok = unsafe { AssignProcessToJobObject(handle, proc) };
            unsafe { CloseHandle(proc) };
            if ok == 0 {
                let error = std::io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                anyhow::bail!("AssignProcessToJobObject({pid}) failed: {error}");
            }
            Ok(Self { handle })
        }

        /// 终止 Job 内所有进程（幂等；Drop 兜底会再执行一次）。
        pub fn terminate_all(&self) {
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(self.handle, 1);
            }
        }
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            self.terminate_all();
            unsafe { CloseHandle(self.handle) };
        }
    }

    /// 恢复挂起进程的主线程（经线程快照定位属主线程）。
    ///
    /// CREATE_SUSPENDED 的进程主线程从未运行——快照中属于该 PID 的
    /// 线程即主线程（挂起进程不可能创建其他线程）。
    pub fn resume_main_thread(pid: u32) -> Result<()> {
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, THREADENTRY32,
            TH32CS_SNAPTHREAD,
        };
        use windows_sys::Win32::System::Threading::{
            OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        };

        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            anyhow::bail!(
                "CreateToolhelp32Snapshot failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut found = false;
        let mut ok = unsafe { Thread32First(snapshot, &mut entry) };
        while ok != 0 {
            if entry.th32OwnerProcessID == pid {
                let thread = unsafe {
                    OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID)
                };
                if !thread.is_null() {
                    unsafe {
                        ResumeThread(thread);
                        CloseHandle(thread);
                    }
                    found = true;
                }
                break;
            }
            ok = unsafe { Thread32Next(snapshot, &mut entry) };
        }
        unsafe { CloseHandle(snapshot) };
        if found {
            Ok(())
        } else {
            anyhow::bail!("no thread found for suspended pid {pid}")
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

    /// Windows：挂起 spawn → Job 归属 → 恢复执行，正常退出。
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

    /// Windows：宽限期超时后 TerminateJobObject 收束（含 cmd 启动的后代）。
    #[cfg(windows)]
    #[tokio::test]
    async fn managed_child_windows_force_stop_kills_tree() {
        // cmd → ping：孙进程经 Job 归属一并终止
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.args(["/C", "ping -n 60 127.0.0.1 >nul"]);
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        let mut child = spawn_managed(&mut cmd).unwrap();

        let outcome = child.stop(Duration::from_millis(500)).await;
        assert!(
            matches!(outcome, StopOutcome::Graceful(_) | StopOutcome::Forced),
            "Windows process tree must be stopped, got: {outcome:?}"
        );
    }
}
