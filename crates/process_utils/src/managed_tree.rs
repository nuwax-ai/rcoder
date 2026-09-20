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

type OwnedWrapper = Box<dyn process_wrap::tokio::ChildWrapper>;
type PendingKill = tokio::task::JoinHandle<(OwnedWrapper, std::io::Result<()>)>;

/// 受管子进程（持有进程组/Job 归属资源）。
pub struct ManagedChild {
    inner: Option<Box<dyn process_wrap::tokio::ChildWrapper>>,
    pending_kill: Option<PendingKill>,
    /// spawn 时固化的进程 ID（组信号/组探测目标）。不能事后从 wrapper 取：
    /// 进程被 reap 后 `Child::id()` 返回 None，会丢组身份。
    pid: Option<u32>,
}

/// 停止结果（cross-platform.md §4：区分正常收尾、强制停止、清理未确认）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// 进程在宽限期内正常退出。
    Graceful(ExitStatus),
    /// 宽限期后强制终止（Unix SIGKILL 进程组 / Windows TerminateJobObject）。
    Forced(ExitStatus),
    /// 无法确认进程退出（kill/wait 失败或超时）。
    Unconfirmed,
}

/// 启动受管子进程（自动归属进程树管理资源，无逃逸窗口）。
pub fn spawn_managed(cmd: tokio::process::Command) -> Result<ManagedChild> {
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
    Ok(ManagedChild {
        inner: Some(child),
        pending_kill: None,
        pid,
    })
}

impl ManagedChild {
    /// 进程 ID（诊断用）。
    pub fn id(&self) -> Option<u32> {
        self.pid
    }

    /// 取走子进程 stdout 管道（业务日志转发；wrapper 透传到内层 Child）。
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.inner.as_mut()?.stdout().take()
    }

    /// 取走子进程 stderr 管道（业务日志转发；wrapper 透传到内层 Child）。
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.inner.as_mut()?.stderr().take()
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
            if !self
                .kill_until(tokio::time::Instant::now() + Duration::from_secs(5))
                .await
            {
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

    /// Keep the exact wrapper future alive across a deadline. process-wrap may
    /// cache root exit before tree reaping finishes, so dropping and re-creating
    /// kill() could turn an incomplete Windows Job drain into cached success.
    async fn kill_until(&mut self, deadline: tokio::time::Instant) -> bool {
        if self.pending_kill.is_none() {
            let Some(mut inner) = self.inner.take() else {
                return false;
            };
            self.pending_kill = Some(tokio::spawn(async move {
                let result = Box::into_pin(inner.kill()).await;
                (inner, result)
            }));
        }
        let Some(task) = self.pending_kill.as_mut() else {
            return false;
        };
        match tokio::time::timeout_at(deadline, task).await {
            Ok(Ok((inner, result))) => {
                self.pending_kill.take();
                self.inner = Some(inner);
                result.is_ok()
            }
            Ok(Err(_)) => {
                // The original wrapper was lost to a task panic; never claim
                // cleanup or manufacture a replacement handle from its PID.
                self.pending_kill.take();
                false
            }
            Err(_) => false, // task and wrapper remain owned by this object
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
            if !self.kill_until(deadline).await && tokio::time::Instant::now() >= deadline {
                return None;
            }
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
            .and_then(|pid| crate::process_group_exists(pid).ok())
            .unwrap_or(true)
    }

    /// 发送优雅停止信号（平台特定）。
    #[cfg(unix)]
    fn send_stop_signal(&mut self) {
        // ProcessGroup::leader() 使子进程自成组长（pgid == pid）→ 组信号
        // 可达全部后代
        if let Some(pid) = self.pid {
            crate::kill_process_group(pid, crate::KillSignal::SIGTERM);
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
    pub async fn wait_root(&mut self) -> Result<ExitStatus> {
        self.inner
            .as_mut()
            .context("managed child is still owned by its in-flight stop")?
            .inner_mut()
            .wait()
            .await
            .context("wait for managed child root process")
    }

    /// 非阻塞查询**根进程**退出状态（root 语义，理由同 [`Self::wait_root`]）。
    pub fn try_wait_root(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.inner
            .as_mut()
            .ok_or_else(|| std::io::Error::other("managed stop remains in flight"))?
            .inner_mut()
            .try_wait()
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[derive(Debug)]
    struct StalledKill {
        child: tokio::process::Child,
        release: Arc<tokio::sync::Notify>,
        calls: Arc<AtomicUsize>,
    }
    impl process_wrap::tokio::ChildWrapper for StalledKill {
        fn inner(&self) -> &dyn process_wrap::tokio::ChildWrapper {
            &self.child
        }
        fn inner_mut(&mut self) -> &mut dyn process_wrap::tokio::ChildWrapper {
            &mut self.child
        }
        fn into_inner(self: Box<Self>) -> Box<dyn process_wrap::tokio::ChildWrapper> {
            Box::new(self.child)
        }
        fn kill(&mut self) -> Box<dyn Future<Output = std::io::Result<()>> + Send + '_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::new(async move {
                self.release.notified().await;
                self.child.wait().await?;
                Ok(())
            })
        }
    }
    #[tokio::test]
    async fn kill_deadline_retains_exact_future_and_wrapper_until_original_drain_finishes() {
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let release = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut owned = ManagedChild {
            inner: Some(Box::new(StalledKill {
                child,
                release: release.clone(),
                calls: calls.clone(),
            })),
            pending_kill: None,
            pid,
        };
        let started = tokio::time::Instant::now();
        assert!(!owned.kill_until(started + Duration::from_millis(30)).await);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(owned.inner.is_none());
        assert!(owned.pending_kill.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            !owned
                .kill_until(tokio::time::Instant::now() + Duration::from_millis(20))
                .await
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "retry must await original future, not cached-root shortcut"
        );
        release.notify_one();
        assert!(
            owned
                .kill_until(tokio::time::Instant::now() + Duration::from_secs(2))
                .await
        );
        assert!(owned.inner.is_some());
        assert!(owned.pending_kill.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
