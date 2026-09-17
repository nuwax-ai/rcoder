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
    wrap.wrap(process_wrap::tokio::JobObject);

    let child = wrap
        .spawn()
        .context("spawn managed child in process group/job")?;
    Ok(ManagedChild { inner: child })
}

impl ManagedChild {
    /// 进程 ID（诊断用）。
    pub fn id(&self) -> Option<u32> {
        self.inner.id()
    }

    /// 优雅停止：发送停止信号 → 等待宽限期 → 强制终止整个进程树。
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

        // 宽限期后强制终止整个进程树（kill 经 wrapper 派发到组/Job）。
        // trait 返回未固定的 Box<dyn Future>——into_pin 后才可 await。
        let kill = Box::into_pin(self.inner.kill());
        if kill.await.is_err() {
            return StopOutcome::Unconfirmed;
        }

        match tokio::time::timeout(Duration::from_secs(5), self.inner.wait()).await {
            Ok(Ok(status)) => StopOutcome::Forced(status),
            Ok(Err(_)) => StopOutcome::Unconfirmed,
            Err(_) => StopOutcome::Unconfirmed,
        }
    }

    /// 发送优雅停止信号（平台特定）。
    #[cfg(unix)]
    fn send_stop_signal(&mut self) {
        // ProcessGroup::leader() 使子进程自成组长（pgid == pid）→ 组信号
        // 可达全部后代
        if let Some(pid) = self.inner.id() {
            process_utils::kill_process_group(pid, process_utils::KillSignal::SIGTERM);
        }
    }

    /// Windows：无可靠跨树优雅信号（GenerateConsoleCtrlEvent 需
    /// CREATE_NEW_PROCESS_GROUP + 控制台进程）。宽限期内自然退出计
    /// Graceful；超时由 kill() 的 TerminateJobObject 收束全树。
    #[cfg(windows)]
    fn send_stop_signal(&mut self) {}

    /// 等待进程退出。
    pub async fn wait(&mut self) -> Result<ExitStatus> {
        self.inner.wait().await.context("wait for managed child")
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
        let status = child.wait().await.unwrap();
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
        let status = child.wait().await.unwrap();
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
        let status = child.wait().await.unwrap();
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
        let status = child.wait().await.unwrap();
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
