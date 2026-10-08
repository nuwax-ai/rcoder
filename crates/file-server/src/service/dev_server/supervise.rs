//! UserApp manifest 编排进程（app-cli）的可监督 Child 句柄（P1-03）。
//!
//! 旧版 `start_dev_manifest` 直接 `drop(child)`：无 wait/reap（退出码只能靠
//! `kill(pid,0)` 探测有无，拿不到真实 `ExitStatus`）、stdout 事件管道
//! fire-and-forget（EOF/读错不上报）、调用方无法区分"进程退出"与"管道静默
//! 结束"。本模块把 Child 的观察收敛到唯一 worker：
//! - wait task 持 Child 收割退出结果（`watch` 广播，多订阅者只读）；
//! - stdout 事件管道的 JoinHandle 被持有，支持**有界排空**（后代继承
//!   stdout 时管道永不 EOF，不无限等待）；
//! - stderr 环形缓冲共享，退出错误可附尾部行。
//!
//! 职责边界：本句柄只负责**观察与收割**；停止仍走进程组信号
//! （`stop_dev` 既有路径），不形成第二 kill 路径。

use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::process::Child;
use tokio::task::JoinHandle;

use super::error_classify::{self, StderrRing};
use super::process::OnLineCallback;

/// stdout/stderr 管道结束原因（`DevEventHooks::on_end` 上报）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEndReason {
    /// 对端关闭（EOF）——进程退出或正常关流。
    Eof,
    /// 读取错误（管道故障/非 UTF-8 字节等）。
    ReadFailed(String),
}

impl std::fmt::Display for StreamEndReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eof => f.write_str("eof"),
            Self::ReadFailed(error) => write!(f, "read failed: {error}"),
        }
    }
}

/// stdout/stderr 管道结束回调（P1-03）。
pub type StreamEndCallback = Arc<dyn Fn(&StreamEndReason) + Send + Sync>;

/// dev 启动的事件钩子（userapp manifest 链路；web/vite 路径传 None）。
///
/// - `on_line`：每原始行去 `APP-CLI-EVT ` 前缀后的 JSON（同旧
///   `OnLineCallback` 语义，SSE 转发）；
/// - `on_end`：stdout 管道结束（EOF/读错）时回调**恰好一次**——调用方据此
///   终结"等编排事件流"的等待（进程退出后管道迟早 EOF；读错则事件通道已
///   不可信），不再只能靠等待窗超时兜底。
#[derive(Clone)]
pub struct DevEventHooks {
    pub on_line: OnLineCallback,
    pub on_end: Option<StreamEndCallback>,
    /// Absolute parent startup deadline, shared by owner discovery and its original operation.
    pub launch_deadline: Option<tokio::time::Instant>,
    /// Called only after the original owner operation was verified as admitted,
    /// or a local child and its launch identity were atomically registered.
    /// The callback releases submission barriers; it must not perform new work.
    pub on_submitted: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl DevEventHooks {
    /// Existing launch policy: configured command budget plus 30s Pingap and
    /// 120s scheduling margin, with a 1200s upper bound.
    pub fn launch_budget(command_budget_secs: u64) -> Duration {
        Duration::from_secs(command_budget_secs.saturating_add(150).min(1200))
    }
    /// 最小钩子（行回调 no-op、无结束回调）——与旧 `None` 行为等价。
    pub fn noop() -> Self {
        Self {
            on_line: Arc::new(|_json: &str| {}),
            on_end: None,
            launch_deadline: None,
            on_submitted: None,
        }
    }
    pub(super) fn submitted(&self) {
        if let Some(callback) = &self.on_submitted {
            callback();
        }
    }
}

/// 子进程退出结果（wait 成功=真实 `ExitStatus`；wait 失败保留原始错误）。
#[derive(Debug, Clone)]
pub enum ChildExit {
    Exited(ExitStatus),
    WaitFailed(String),
}

impl ChildExit {
    /// 人读描述（错误信息嵌入用；不带 argv/env——诊断不泄露敏感面）。
    pub fn describe(&self) -> String {
        match self {
            Self::Exited(status) => match status.code() {
                Some(code) => format!("exit code {code}"),
                None => "terminated by signal".to_string(),
            },
            Self::WaitFailed(error) => format!("wait failed: {error}"),
        }
    }
}

/// 被监督的编排子进程句柄（Arc 共享；观察只读，收割由内部 worker 独占）。
pub struct SupervisedChild {
    pid: u32,
    exit: tokio::sync::watch::Receiver<Option<ChildExit>>,
    stdout_task: Mutex<Option<JoinHandle<()>>>,
    stderr_ring: Arc<StderrRing>,
}

impl SupervisedChild {
    /// 收编 Child：spawn 唯一 wait/reap worker，退出结果经 watch 广播。
    /// stdout 管道 JoinHandle 随后经 [`Self::attach_stdout`] 挂载。
    pub fn adopt(mut child: Child, stderr_ring: Arc<StderrRing>) -> Arc<Self> {
        let pid = child.id().unwrap_or(0);
        let (exit_tx, exit_rx) = tokio::sync::watch::channel(None);
        tokio::spawn(async move {
            // 唯一 wait/reap 点：真实 ExitStatus；wait 罕见 io 错误保留原文。
            let exit = match child.wait().await {
                Ok(status) => ChildExit::Exited(status),
                Err(error) => ChildExit::WaitFailed(error.to_string()),
            };
            drop(exit_tx.send(Some(exit)));
        });
        Arc::new(Self {
            pid,
            exit: exit_rx,
            stdout_task: Mutex::new(None),
            stderr_ring,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 退出快照（非阻塞）：未退出或 worker 意外终止时 None。
    pub fn exited(&self) -> Option<ChildExit> {
        self.exit.borrow().clone()
    }

    /// 有界等待退出：超时未退出 → None（不 kill、不误判）。
    pub async fn wait_exit(&self, timeout: Duration) -> Option<ChildExit> {
        let mut rx = self.exit.clone();
        if let Some(exit) = rx.borrow().clone() {
            return Some(exit);
        }
        match tokio::time::timeout(timeout, rx.changed()).await {
            Ok(Ok(())) => rx.borrow().clone(),
            _ => None,
        }
    }

    /// stderr 环形缓冲尾部（早退分类/错误附录用）。
    pub fn stderr_tail(&self) -> Vec<String> {
        error_classify::ring_collect(&self.stderr_ring)
    }

    /// 挂载 stdout 事件管道 task（供有界排空）。
    pub fn attach_stdout(&self, handle: JoinHandle<()>) {
        *self
            .stdout_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    /// 有界排空 stdout 管道：等待管道 task 结束（EOF/读错）至多 `window`。
    /// 后代继承 stdout 时管道永不 EOF——超时放弃（管道 task 继续写日志，
    /// 不 kill、不算失败）。handle 取出后锁即释放，await 不持锁。
    pub async fn drain_stdout(&self, window: Duration) {
        let handle = self
            .stdout_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut handle) = handle
            && tokio::time::timeout(window, &mut handle).await.is_err()
        {
            tracing::warn!("stdout drain window exceeded; a descendant may hold the pipe open");
        }
    }
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn ring() -> Arc<StderrRing> {
        Arc::new(Mutex::new(VecDeque::new()))
    }

    fn spawn_child(program: &str, args: &[&str]) -> Child {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command.spawn().expect("spawn test child")
    }

    /// 收编后能收割真实退出码：wait_exit 未退出窗口内 None，退出后
    /// Exited(code)。
    #[tokio::test]
    async fn adopt_reaps_real_exit_status() {
        let child = spawn_child("/bin/sh", &["-c", "sleep 5"]);
        let supervised = SupervisedChild::adopt(child, ring());
        assert!(
            supervised
                .wait_exit(Duration::from_millis(100))
                .await
                .is_none(),
            "child must still be running"
        );
        assert!(supervised.exited().is_none());
        // SIGKILL 直杀（不经 stop 路径）——watcher 必须能观察到并收割。
        let pid = supervised.pid() as i32;
        drop(
            tokio::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status()
                .await,
        );
        let exit = supervised
            .wait_exit(Duration::from_secs(5))
            .await
            .expect("exit must be observed after kill");
        assert!(
            matches!(exit, ChildExit::Exited(_)),
            "killed child must report an exit status: {exit:?}"
        );
    }

    /// 立即退出的子进程（退出码 0 与非 0 都可收割）。
    #[tokio::test]
    async fn short_lived_child_exit_is_observed() {
        for code in [0, 3] {
            let child = spawn_child("/bin/sh", &["-c", &format!("exit {code}")]);
            let supervised = SupervisedChild::adopt(child, ring());
            let exit = supervised
                .wait_exit(Duration::from_secs(5))
                .await
                .expect("exit observed");
            assert_eq!(exit.describe(), format!("exit code {code}"));
        }
    }

    /// drain_stdout 有界：stdout 被后代长持（sleep 持续写端）时窗口到期放弃。
    #[tokio::test]
    async fn drain_stdout_is_bounded_when_pipe_stays_open() {
        // 父进程立即退出，孙进程继承 stdout 并长睡——管道不 EOF。
        let child = spawn_child("/bin/sh", &["-c", "(sleep 30 &) ; exit 0"]);
        let supervised = SupervisedChild::adopt(child, ring());
        supervised.attach_stdout(tokio::spawn(async {})); // 占位已结束的管道 task
        let start = std::time::Instant::now();
        supervised.drain_stdout(Duration::from_millis(200)).await;
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "drain must honour the bounded window"
        );
    }

    #[tokio::test]
    async fn reaped_launcher_cleanup_never_signals_reused_pid() {
        let status = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .status()
            .await
            .unwrap();
        launcher_cleanup_signal_case(ChildExit::Exited(status), true).await;
    }

    #[tokio::test]
    async fn unreadable_launcher_exit_never_signals_unconfirmed_pid() {
        launcher_cleanup_signal_case(ChildExit::WaitFailed("reaper failed".into()), false).await;
    }

    /// Model PID reuse with a real unrelated process and a retained reaped
    /// watch. Exercise the actual cleanup signal path, then reap our fixture
    /// before any assertions so the red counterexample leaves no child behind.
    async fn launcher_cleanup_signal_case(exit: ChildExit, expect_retired: bool) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::Config::from_env().unwrap();
        config.log_base_dir = dir.path().join("logs");
        config.dev_stop_check_interval_ms = 10;
        config.dev_stop_max_attempts = 1;
        let manager = super::super::DevServerManager::new(Arc::new(config));
        let mut victim = tokio::process::Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = victim.id().unwrap();
        let (sender, receiver) = tokio::sync::watch::channel(Some(exit));
        drop(sender);
        let supervised = Arc::new(SupervisedChild {
            pid,
            exit: receiver,
            stdout_task: Mutex::new(None),
            stderr_ring: ring(),
        });
        let key = "userapp:reused-pid";
        manager.launches.lock().unwrap().insert(
            key.into(),
            super::super::types::LocalLaunch {
                launch_id: "original-launch".into(),
                state_root: None,
            },
        );
        manager.processes.lock().unwrap().insert(
            key.into(),
            crate::models::DevProcess {
                pid,
                port: shared_types::APP_ENTRY_PORT,
                project_id: key.into(),
                instance_id: None,
                base_path: None,
                started_at: 0,
                log_dir: dir.path().join("logs"),
                temp_log_name: String::new(),
                external_owner: None,
            },
        );
        manager
            .supervised
            .lock()
            .unwrap()
            .insert(key.into(), supervised);
        let result = manager
            .close_local_registration(key, Some("original-launch"))
            .await;
        let survived = victim.try_wait().unwrap().is_none();
        if survived {
            victim.kill().await.unwrap();
        }
        victim.wait().await.unwrap();

        assert!(
            survived,
            "a retained launcher record must never signal this unrelated live process"
        );
        if expect_retired {
            assert!(
                result.unwrap().is_empty(),
                "an already-reaped launcher needs no signal"
            );
            assert!(!manager.processes.lock().unwrap().contains_key(key));
            assert!(!manager.launches.lock().unwrap().contains_key(key));
            assert!(!manager.supervised.lock().unwrap().contains_key(key));
        } else {
            assert!(
                result.is_err(),
                "unreadable exit status cannot confirm cleanup"
            );
            assert!(manager.processes.lock().unwrap().contains_key(key));
            assert!(manager.launches.lock().unwrap().contains_key(key));
            assert!(manager.supervised.lock().unwrap().contains_key(key));
            assert!(matches!(
                manager.cleanup_state.lock().unwrap().get(key),
                Some(super::super::types::CleanupStatus::Cleaning)
            ));
        }
    }
}
