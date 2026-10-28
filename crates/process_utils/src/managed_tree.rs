//! 跨平台进程树管理（cross-platform.md §4）。
//!
//! 保证：受管进程的所有后代都随停止操作退出，不遗留孤儿进程。
//!
//! `process-wrap` provides atomic spawn ownership (Unix group / Windows Job).
//! A POSIX PGID is a numeric locator, not a stable kernel handle: Unix keeps
//! its original Child waitable with WNOWAIT until tree quiescence is proven,
//! then commits that proof before reaping. Windows retains its original Job.
//! Spawn wrappers (watchexec's successor to command-group):
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
#[cfg(any(windows, test))]
type PendingKill = tokio::task::JoinHandle<(OwnedWrapper, std::io::Result<()>)>;

/// 受管子进程（持有进程组/Job 归属资源）。
pub struct ManagedChild {
    inner: Option<OwnedWrapper>,
    #[cfg(any(windows, test))]
    pending_kill: Option<PendingKill>,
    /// Once the original tree is proven inactive, its numeric PGID is never
    /// probed or signalled again. The original root alone is reaped afterward.
    quiescent_exit: Option<ExitStatus>,
    #[cfg(unix)]
    observed_root_exit: Option<ExitStatus>,
    root_reaped: bool,
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
        #[cfg(any(windows, test))]
        pending_kill: None,
        quiescent_exit: None,
        #[cfg(unix)]
        observed_root_exit: None,
        root_reaped: false,
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
    /// - Unix：root 已退出**且进程组内无活执行成员**（组信号可达性与父子关系
    ///   无关——root 先死时孙进程被 reparent 到 init，waitpid 视角 ECHILD
    ///   不构成收束证据，必须组探测）；
    /// - Windows：wrapper `kill()`（TerminateJobObject + Job 空仓收束循环）
    ///   返回即整树收束（Job 归属不随 reparent 改变）。
    ///
    /// 宽限期与确认路径都不能调用 wrapper 的树级 `wait()`——它会把 root 状态
    /// 写入缓存，使后续 `kill()` 内部的收束循环短路。
    pub async fn stop(&mut self, grace_period: Duration) -> StopOutcome {
        if self.quiescent_exit.is_some() {
            return self
                .finish_confirmed_root()
                .await
                .map_or(StopOutcome::Unconfirmed, StopOutcome::Graceful);
        }
        // 第一步：发送优雅停止信号（Windows 无跨树信号，为 no-op）
        self.send_stop_signal();

        // 第二步：宽限期内等整树收束
        #[cfg(unix)]
        if let Some(status) = self.await_tree_exit(grace_period).await {
            return StopOutcome::Graceful(status);
        }
        #[cfg(windows)]
        let root_within_grace = tokio::time::timeout(grace_period, self.wait_root()).await;

        // Force the original pinned POSIX group, or the original Windows Job.
        // Unix must not use wrapper tree-wait/kill to release the PID pin early.
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

    /// Stop within a caller-owned absolute deadline. The original wrapper and
    /// an in-flight tree drain remain owned on timeout, so the same stop can be
    /// observed again without manufacturing a new PID/Job handle.
    pub async fn stop_until(
        &mut self,
        grace_period: Duration,
        deadline: tokio::time::Instant,
    ) -> StopOutcome {
        if self.quiescent_exit.is_some() {
            return self
                .finish_confirmed_root()
                .await
                .map_or(StopOutcome::Unconfirmed, StopOutcome::Graceful);
        }
        // Observe quiescence before signalling a group whose root may already
        // have been reaped. A completed tree never needs another numeric signal.
        #[cfg(unix)]
        if let Some(status) = self.await_tree_exit(Duration::ZERO).await {
            return StopOutcome::Graceful(status);
        }
        if tokio::time::Instant::now() >= deadline {
            return StopOutcome::Unconfirmed;
        }
        self.send_stop_signal();
        let grace =
            grace_period.min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        #[cfg(unix)]
        if let Some(status) = self.await_tree_exit(grace).await {
            return StopOutcome::Graceful(status);
        }
        #[cfg(windows)]
        let root_within_grace = tokio::time::timeout(grace, self.wait_root()).await;
        #[cfg(unix)]
        {
            match self.kill_tree_and_confirm_until(deadline).await {
                Some(status) => StopOutcome::Forced(status),
                None => StopOutcome::Unconfirmed,
            }
        }
        #[cfg(windows)]
        {
            if !self.kill_until(deadline).await {
                return StopOutcome::Unconfirmed;
            }
            match root_within_grace {
                Ok(Ok(status)) => StopOutcome::Graceful(status),
                Ok(Err(_)) | Err(_) => match self.try_wait_root() {
                    Ok(Some(status)) => StopOutcome::Forced(status),
                    Ok(None) | Err(_) => StopOutcome::Unconfirmed,
                },
            }
        }
    }

    /// Windows: keep the exact wrapper future alive across a deadline. process-wrap may
    /// cache root exit before tree reaping finishes, so dropping and re-creating
    /// kill() could turn an incomplete Windows Job drain into cached success.
    #[cfg(any(windows, test))]
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

    /// Unix force stop retains its original waitable root as a PID/PGID pin
    /// until no executing group member remains, then reaps only that Child.
    ///
    /// A readable all-zombie group has no executing member, including Darwin's
    /// zombie-only killpg(0) EPERM. Live/inaccessible/unknown observations remain
    /// unconfirmed. Reap only the original root after the terminal proof.
    #[cfg(unix)]
    async fn kill_tree_and_confirm(&mut self) -> Option<ExitStatus> {
        self.kill_tree_and_confirm_until(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
    }

    #[cfg(unix)]
    async fn kill_tree_and_confirm_until(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> Option<ExitStatus> {
        // The original root is still waitable (NOWAIT), so its PID/PGID cannot
        // be allocated to a foreign group between observation and this signal.
        if self.try_wait_root().is_err() {
            return None;
        }
        if let Some(pid) = self.pid {
            crate::kill_process_group(pid, crate::KillSignal::SIGKILL);
        }
        self.await_tree_exit(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await
    }

    /// Observe root exit plus no active group member, commit terminal proof
    /// before releasing the original waitable root/PID identity pin.
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
                && !self.process_group_alive(deadline).await
            {
                self.quiescent_exit = Some(status);
                return self.finish_confirmed_root().await;
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + Duration::from_millis(20)),
            )
            .await;
        }
    }

    /// 进程组是否仍有存活成员（观察失败按存活处理——保守不误报收束）。
    #[cfg(unix)]
    async fn process_group_alive(&self, deadline: tokio::time::Instant) -> bool {
        let Some(pid) = self.pid else {
            return true;
        };
        match tokio::time::timeout_at(deadline, crate::execution_tree::has_live_group_members(pid))
            .await
        {
            Ok(Ok(live)) => live,
            Ok(Err(_)) | Err(_) => true,
        }
    }

    /// 发送优雅停止信号（平台特定）。
    #[cfg(unix)]
    fn send_stop_signal(&mut self) {
        // ProcessGroup::leader() 使子进程自成组长（pgid == pid）→ 组信号
        // 可达全部后代
        if self.quiescent_exit.is_none()
            && self.try_wait_root().is_ok()
            && let Some(pid) = self.pid
        {
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
    /// Root exit triggers subsequent tree cleanup. On Unix, observing exit
    /// deliberately retains its kernel PID pin until Stop confirms no live
    /// group member, then reaps it. Waiting for descendants here would hide a
    /// failed root from the existing supervisor/restart loop.
    #[cfg(unix)]
    pub async fn wait_root(&mut self) -> Result<ExitStatus> {
        let mut child_signal =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
                .context("observe original child exit signal")?;
        loop {
            if let Some(status) = self
                .try_wait_root()
                .context("observe original child exit without reaping its identity pin")?
            {
                return Ok(status);
            }
            child_signal
                .recv()
                .await
                .context("original child exit observer closed")?;
        }
    }

    #[cfg(windows)]
    pub async fn wait_root(&mut self) -> Result<ExitStatus> {
        self.inner
            .as_mut()
            .context("managed child is still owned by its in-flight stop")?
            .inner_mut()
            .wait()
            .await
            .context("wait for managed child root process")
    }

    /// Unix observes with WNOWAIT, retaining the actual original Child/PID as
    /// the group identity pin until Stop proves that no execution remains. This
    /// is not a live-process/admission marker: a zombie never counts as work.
    #[cfg(unix)]
    pub fn try_wait_root(&mut self) -> std::io::Result<Option<ExitStatus>> {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        use std::os::unix::process::ExitStatusExt;
        if let Some(status) = self.observed_root_exit {
            return Ok(Some(status));
        }
        let pid = self
            .pid
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| *pid > 1)
            .and_then(Pid::from_raw)
            .ok_or_else(|| {
                std::io::Error::other("original child process identifier is unavailable")
            })?;
        let observed = waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        )
        .map_err(std::io::Error::from)?;
        let Some(observed) = observed else {
            return Ok(None);
        };
        let status = if let Some(code) = observed.exit_status() {
            ExitStatus::from_raw(code << 8)
        } else if let Some(signal) = observed.terminating_signal() {
            ExitStatus::from_raw(signal | if observed.dumped() { 0x80 } else { 0 })
        } else {
            return Err(std::io::Error::other(
                "original child exit observation has no terminal status",
            ));
        };
        self.observed_root_exit = Some(status);
        Ok(Some(status))
    }

    #[cfg(windows)]
    pub fn try_wait_root(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.inner
            .as_mut()
            .ok_or_else(|| std::io::Error::other("managed stop remains in flight"))?
            .inner_mut()
            .try_wait()
    }

    async fn finish_confirmed_root(&mut self) -> Option<ExitStatus> {
        let status = self.quiescent_exit?;
        if !self.root_reaped {
            let root = self.inner.as_mut()?;
            if root.inner_mut().wait().await.is_err() {
                return None;
            }
            self.root_reaped = true;
        }
        Some(status)
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
            quiescent_exit: None,
            #[cfg(unix)]
            observed_root_exit: None,
            root_reaped: false,
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

#[cfg(all(test, unix))]
mod identity_pin_tests {
    use super::*;
    use std::time::Duration;

    /// A real observed exit must remain waitable until this same tree is
    /// confirmed inactive. The reserved root PID makes foreign PGID reuse
    /// impossible during TERM/KILL/observation, without a new persistent marker.
    #[tokio::test]
    async fn original_exit_keeps_kernel_identity_pin_until_tree_confirmation_and_reap() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let mut owned = spawn_managed(command).unwrap();
        let pid = owned.id().unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(2), owned.wait_root())
            .await
            .unwrap()
            .unwrap();
        let slot_pinned = crate::process_exists(pid).unwrap();
        let actual_exit = owned.try_wait_root().unwrap();
        let stopped = owned.stop(Duration::from_millis(200)).await;
        let slot_released = !crate::process_exists(pid).unwrap();
        assert!(exit.success());
        assert!(
            slot_pinned,
            "root exit observation must not release its kernel PID pin before tree confirmation"
        );
        assert!(actual_exit.is_some_and(|status| status.success()));
        assert!(matches!(
            stopped,
            StopOutcome::Graceful(_) | StopOutcome::Forced(_)
        ));
        assert!(
            slot_released,
            "successful Stop must reap the original owned root"
        );
    }

    /// Controlled numeric-reuse injection after a REAL old tree was stopped.
    /// We do not try to force the OS allocator to reuse a PID. The stale numeric
    /// locator is replaced with a real independent group's PID; a late old Stop
    /// must return its original receipt and send no TERM/KILL to that group.
    #[tokio::test]
    async fn confirmed_old_tree_never_signals_a_foreign_group_reusing_its_numeric_locator() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let mut old = spawn_managed(command).unwrap();
        assert!(matches!(
            old.stop(Duration::from_millis(200)).await,
            StopOutcome::Graceful(_) | StopOutcome::Forced(_)
        ));
        let mut foreign = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        let foreign_pid = foreign.id().unwrap();
        old.pid = Some(foreign_pid);
        let late = old.stop(Duration::from_millis(20)).await;
        let late_deadline = old
            .stop_until(
                Duration::ZERO,
                tokio::time::Instant::now() + Duration::from_millis(20),
            )
            .await;
        let foreign_alive = foreign.try_wait().unwrap().is_none();
        if foreign_alive {
            foreign.kill().await.unwrap();
        }
        let _exit = foreign.wait().await.unwrap();
        assert!(
            foreign_alive,
            "late old Stop signalled an unrelated reused group locator"
        );
        assert!(matches!(
            late,
            StopOutcome::Graceful(_) | StopOutcome::Forced(_)
        ));
        assert!(matches!(
            late_deadline,
            StopOutcome::Graceful(_) | StopOutcome::Forced(_)
        ));
    }
}
