//! 跨平台进程树管理（cross-platform.md §4）。
//!
//! 保证：受管进程的所有后代都随停止操作退出，不遗留孤儿进程。
//!
//! 底层归属与树杀由 `process-wrap`（watchexec 维护，command-group 官方
//! 后继）承担：
//! - Unix：`ProcessGroup::leader()`（子进程自成组长，spawn 原子归属）
//! - Windows：`JobObject`——挂起 spawn → Job 归属 → 恢复执行（源码
//!   windows.rs：`flags | CREATE_SUSPENDED` + assign 后 resume，进程
//!   开始运行前已入 Job，无后代逃逸窗口）
//!
//! [`ManagedChild`] 在其上补业务语义：优雅信号 → 宽限期 → 全树强杀
//! 三段式停止与 `StopOutcome` 三态收束。

use anyhow::{Context, Result};
use std::process::ExitStatus;
use std::time::Duration;

/// 受管子进程（持有进程组/Job 归属资源）。
pub(crate) struct ManagedChild {
    inner: Box<dyn process_wrap::tokio::ChildWrapper>,
    /// spawn 时固化的进程 ID（组信号/组探测目标）。不能事后从 wrapper 取：
    /// 进程被 reap 后 `Child::id()` 返回 None，会丢组身份。
    pid: Option<u32>,
}

/// 停止结果（cross-platform.md §4：区分正常收尾、强制停止、清理未确认）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    /// 进程在宽限期内正常退出。
    Graceful(ExitStatus),
    /// 宽限期后强制终止（Unix SIGKILL 进程组 / Windows TerminateJobObject）。
    Forced(ExitStatus),
    /// 无法确认进程退出（kill/wait 失败或超时）。
    Unconfirmed,
}

/// 启动受管子进程（自动归属进程树管理资源，无逃逸窗口）。
pub(crate) fn spawn_managed(cmd: tokio::process::Command) -> Result<ManagedChild> {
    use process_wrap::tokio::CommandWrap;

    let mut wrap = CommandWrap::from(cmd);
    #[cfg(unix)]
    wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    {
        // JobObject alone tracks descendants but does not set KILL_ON_JOB_CLOSE.
        // KillOnDrop opts the job into OS cleanup even when the owner crashes
        // and Rust destructors never run (TerminateProcess / process abort).
        wrap.wrap(process_wrap::tokio::KillOnDrop);
        wrap.wrap(process_wrap::tokio::JobObject);
    }

    let child = wrap
        .spawn()
        .context("spawn managed child in process group/job")?;
    let pid = child.id();
    Ok(ManagedChild { inner: child, pid })
}

impl ManagedChild {
    /// 进程 ID（诊断用）。
    pub fn id(&self) -> Option<u32> {
        self.pid
    }

    /// 取走子进程 stdout 管道（业务日志转发；wrapper 透传到内层 Child）。
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.inner.stdout().take()
    }

    /// 取走子进程 stderr 管道（业务日志转发；wrapper 透传到内层 Child）。
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.inner.stderr().take()
    }

    /// 优雅停止：发送停止信号 → 宽限期 → 强制终止整个进程树，并确认收束。
    ///
    /// `grace_period` 为业务进程的正常退出等待时间。收束确认语义：
    /// - Unix：root 已退出**且进程组内无存活成员**（组信号可达性与父子关系
    ///   无关——root 先死时孙进程被 reparent 到 init，waitpid 视角 ECHILD
    ///   不构成收束证据，必须组探测）；
    /// - Windows：wrapper `kill()`（TerminateJobObject + Job 空仓收束循环）
    ///   返回即整树收束（Job 归属不随 reparent 改变）。
    ///
    /// 宽限期与确认路径都不能调用 wrapper 的树级 `wait()`——它会把 root 状态
    /// 写入缓存，使后续 `kill()` 内部的收束循环短路。
    pub async fn stop(&mut self, grace_period: Duration) -> StopOutcome {
        // 第一步：发送优雅停止信号（Windows 无跨树信号，为 no-op）
        self.send_stop_signal();

        // 第二步：宽限期内等整树收束
        #[cfg(unix)]
        if let Some(status) = self.await_tree_exit(grace_period).await {
            return StopOutcome::Graceful(status);
        }
        #[cfg(windows)]
        let root_within_grace = tokio::time::timeout(grace_period, self.wait_root()).await;

        // 第三步：强制终止整个进程树（kill 经 wrapper 派发到组/Job：
        // Unix SIGKILL 进程组 + 收束 reap；Windows TerminateJobObject + Job 收束）。
        #[cfg(unix)]
        {
            match self.kill_tree_and_confirm().await {
                Some(status) => StopOutcome::Forced(status),
                None => StopOutcome::Unconfirmed,
            }
        }
        #[cfg(windows)]
        {
            let kill = Box::into_pin(self.inner.kill());
            if kill.await.is_err() {
                return StopOutcome::Unconfirmed;
            }
            match root_within_grace {
                Ok(Ok(status)) => StopOutcome::Graceful(status),
                _ => match self.try_wait_root() {
                    Ok(Some(status)) => StopOutcome::Forced(status),
                    _ => StopOutcome::Unconfirmed,
                },
            }
        }
    }

    /// Unix 强杀整树并有界确认（root 已 reap 且进程组 ESRCH）。
    ///
    /// macOS 僵尸窗口：TERM 杀死成员后、收尸完成前，对"仅剩僵尸"的组
    /// `killpg(SIGKILL)` 返回 **EPERM** 而非 Linux 的成功语义，且 start_kill
    /// 失败使 wrapper kill 不进内部收束 reap——僵尸无人收尸则组探测永真。
    /// 因此确认循环每轮复调 wrapper kill：start_kill 幂等（ESRCH/EPERM 均无
    /// 害），内部 wait 的 reap 循环（waitpid(-pgid)）会收掉同组僵尸，收尸后
    /// 组探测即 ESRCH。真正的 EPERM（无法送达的存活成员）由 deadline 兜底
    /// 为收束未确认。
    #[cfg(unix)]
    async fn kill_tree_and_confirm(&mut self) -> Option<ExitStatus> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut root_status: Option<ExitStatus> = None;
        loop {
            let _ = Box::into_pin(self.inner.kill()).await;
            if root_status.is_none() {
                root_status = self.try_wait_root().ok().flatten();
            }
            if let Some(status) = root_status
                && !self.process_group_alive()
            {
                return Some(status);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 有界轮询整树收束（Unix）：root 已 reap 且进程组内无存活成员。
    /// 超时返回 None。等价旧 supervisor 的 wait_for_quiescence 双条件。
    #[cfg(unix)]
    async fn await_tree_exit(&mut self, budget: Duration) -> Option<ExitStatus> {
        let deadline = tokio::time::Instant::now() + budget;
        let mut root_status: Option<ExitStatus> = None;
        loop {
            if root_status.is_none() {
                root_status = self.try_wait_root().ok().flatten();
            }
            if let Some(status) = root_status
                && !self.process_group_alive()
            {
                return Some(status);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 进程组是否仍有存活成员（观察失败按存活处理——保守不误报收束）。
    #[cfg(unix)]
    fn process_group_alive(&self) -> bool {
        self.pid
            .and_then(|pid| process_utils::process_group_exists(pid).ok())
            .unwrap_or(true)
    }

    /// 发送优雅停止信号（平台特定）。
    #[cfg(unix)]
    fn send_stop_signal(&mut self) {
        // ProcessGroup::leader() 使子进程自成组长（pgid == pid）→ 组信号
        // 可达全部后代
        if let Some(pid) = self.pid {
            process_utils::kill_process_group(pid, process_utils::KillSignal::SIGTERM);
        }
    }

    /// Windows：无可靠跨树优雅信号（GenerateConsoleCtrlEvent 需
    /// CREATE_NEW_PROCESS_GROUP + 控制台进程）。宽限期内自然退出计
    /// Graceful；超时由 kill() 的 TerminateJobObject 收束全树。
    #[cfg(windows)]
    fn send_stop_signal(&mut self) {}

    /// 只等**根进程**退出，不等整树收束。
    ///
    /// 服务退出检测与迁移状态捕获需要旧 `tokio::process::Child` 的 root 语义：
    /// 根进程退出即触发后续全组清理——若用树级等待，一个挂死的孙进程会掩盖
    /// 根进程死亡（监督循环不触发重启）。树级收束确认由 [`Self::stop`] 承担
    /// （信号 → 宽限 → 整树强杀 → 确认）。
    pub(crate) async fn wait_root(&mut self) -> Result<ExitStatus> {
        self.inner
            .inner_mut()
            .wait()
            .await
            .context("wait for managed child root process")
    }

    /// 非阻塞查询**根进程**退出状态（root 语义，理由同 [`Self::wait_root`]）。
    pub(crate) fn try_wait_root(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.inner.inner_mut().try_wait()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silent(mut cmd: tokio::process::Command) -> tokio::process::Command {
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
        cmd
    }

    /// 受管进程的后代随父进程组停止而退出（XP05 Unix 真实进程组）。
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_process_group_stop() {
        let mut cmd = silent(tokio::process::Command::new("/bin/sh"));
        cmd.args(["-c", "sleep 60 & sleep 60 & wait"]);
        let mut child = spawn_managed(cmd).unwrap();
        let pid = child.id().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let outcome = child.stop(Duration::from_secs(1)).await;
        assert!(
            matches!(outcome, StopOutcome::Graceful(_) | StopOutcome::Forced(_)),
            "process group must be stopped, got: {outcome:?}"
        );

        // 确认进程组中的所有进程都已退出
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !process_utils::process_group_exists(pid).unwrap_or(true),
            "process group {pid} should not exist after stop"
        );
    }

    /// 等待受管进程正常退出。
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_normal_exit() {
        let mut cmd = silent(tokio::process::Command::new("/bin/sh"));
        cmd.args(["-c", "exit 0"]);
        let mut child = spawn_managed(cmd).unwrap();
        let status = child.wait_root().await.unwrap();
        assert!(status.success());
    }

    /// 强制终止：忽略 SIGTERM 的进程在宽限期后被杀。
    #[cfg(unix)]
    #[tokio::test]
    async fn managed_child_force_kill_after_timeout() {
        let mut cmd = silent(tokio::process::Command::new("python3"));
        cmd.args([
            "-c",
            "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)",
        ]);
        let mut child = spawn_managed(cmd).unwrap();

        let outcome = child.stop(Duration::from_millis(150)).await;
        assert!(
            matches!(outcome, StopOutcome::Graceful(_) | StopOutcome::Forced(_)),
            "process must be stopped, got: {outcome:?}"
        );
    }

    /// XP08（Unix）：空格 + 中文路径的工作目录与参数下受管 spawn/停止正常。
    #[cfg(unix)]
    #[tokio::test]
    async fn xp08_managed_child_with_spaces_and_unicode_paths() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("项目 目录 with spaces");
        std::fs::create_dir_all(&workdir).unwrap();
        let marker = workdir.join("标记 文件.txt");

        let mut cmd = silent(tokio::process::Command::new("/bin/sh"));
        cmd.current_dir(&workdir);
        cmd.args(["-c", &format!("echo ok > '{}'", marker.display())]);
        let mut child = spawn_managed(cmd).unwrap();
        let status = child.wait_root().await.unwrap();
        assert!(status.success(), "spawn in unicode+spaces cwd must succeed");
        assert!(
            marker.exists(),
            "command must run inside the unicode workdir"
        );
    }

    /// XP08（Windows）：空格 + 中文路径的工作目录 + cmd /C 受管 spawn。
    ///
    /// cmd /C 的整条命令必须经 raw_arg 原样传入——args() 对含空格参数
    /// 自动加引号会与重定向语法嵌套破坏命令行（cross-platform.md §5
    /// 的 Windows shell 命令适配坑，真实业务同样适用）。
    #[cfg(windows)]
    #[tokio::test]
    async fn xp08_managed_child_with_spaces_and_unicode_paths() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("项目 目录 with spaces");
        std::fs::create_dir_all(&workdir).unwrap();
        let marker = workdir.join("标记 文件.txt");

        let mut cmd = silent(tokio::process::Command::new("cmd"));
        cmd.current_dir(&workdir);
        {
            use std::os::windows::process::CommandExt;
            cmd.raw_arg(format!("/C echo ok>\"{}\"", marker.display()));
        }
        let mut child = spawn_managed(cmd).unwrap();
        let status = child.wait_root().await.unwrap();
        assert!(
            status.success(),
            "spawn in unicode+spaces cwd must succeed (status: {status:?})"
        );
        assert!(
            marker.exists(),
            "command must run inside the unicode workdir"
        );
    }

    /// Windows：挂起 spawn → Job 归属 → 恢复执行，正常退出。
    #[cfg(windows)]
    #[tokio::test]
    async fn managed_child_windows_spawn_and_exit() {
        let mut cmd = silent(tokio::process::Command::new("cmd"));
        cmd.args(["/C", "exit 0"]);
        let mut child = spawn_managed(cmd).unwrap();
        let status = child.wait_root().await.unwrap();
        assert!(status.success());
    }

    /// Windows：宽限期超时后 Job 终止收束全树（cmd → ping 孙进程）。
    #[cfg(windows)]
    #[tokio::test]
    async fn managed_child_windows_force_stop_kills_tree() {
        let mut cmd = silent(tokio::process::Command::new("cmd"));
        cmd.args(["/C", "ping -n 60 127.0.0.1 >nul"]);
        let mut child = spawn_managed(cmd).unwrap();

        let outcome = child.stop(Duration::from_millis(500)).await;
        assert!(
            matches!(outcome, StopOutcome::Graceful(_) | StopOutcome::Forced(_)),
            "Windows process tree must be stopped, got: {outcome:?}"
        );
    }
}
