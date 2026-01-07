//! Agent生命周期管理
//!
//! 基于RAII原则的简洁生命周期管理设计

use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use sacp::schema::SessionId;
use shared_types::AgentLifecycle;

/// Agent生命周期守卫
///
/// 遵循RAII原则，当守卫被drop时自动清理agent资源
pub struct AgentLifecycleGuard {
    inner: Arc<AgentLifecycleInner>,
}

impl std::fmt::Debug for AgentLifecycleGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLifecycleGuard")
            .field("project_id", &self.inner.project_id)
            .field("session_id", &self.inner.session_id)
            .field("stopped", &self.inner.stopped.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

struct AgentLifecycleInner {
    project_id: String,
    session_id: SessionId,
    cancel_token: CancellationToken,
    resources: AgentResources,
    stopped: AtomicBool,
}

/// Agent资源管理枚举
enum AgentResources {
    Claude {
        child_process: Arc<Mutex<Option<tokio::process::Child>>>,
        /// stderr 任务 handle（保持任务生命周期）
        #[allow(dead_code)]
        stderr_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    },
}

impl AgentLifecycleGuard {
    /// 为 Claude Agent 创建生命周期守卫
    pub fn new_claude(
        project_id: String,
        session_id: SessionId,
        child_process: tokio::process::Child,
        stderr_task: JoinHandle<()>,
        cancel_token: CancellationToken,
    ) -> Self {
        let resources = AgentResources::Claude {
            child_process: Arc::new(Mutex::new(Some(child_process))),
            stderr_task: Arc::new(Mutex::new(Some(stderr_task))),
        };

        let inner = Arc::new(AgentLifecycleInner {
            project_id,
            session_id,
            cancel_token,
            resources,
            stopped: AtomicBool::new(false),
        });

        Self { inner }
    }

    /// 优雅停止agent
    ///
    /// 带超时机制（5秒），超时后强制 kill 子进程
    pub async fn graceful_stop(&self) -> Result<()> {
        if self.inner.stopped.load(Ordering::SeqCst) {
            info!("Agent already stopped, skipping graceful stop");
            return Ok(());
        }

        info!(
            "Gracefully stopping Claude agent for project: {}",
            self.inner.project_id
        );

        // 1. 发送取消信号
        self.inner.cancel_token.cancel();

        // 2. 根据资源类型执行相应的清理操作
        match &self.inner.resources {
            AgentResources::Claude { child_process, .. } => {
                let mut child_guard = child_process.lock().await;
                if let Some(mut child) = child_guard.take() {
                    info!("Stopping Claude child process");

                    // 设置超时时间：5 秒
                    let timeout_duration = tokio::time::Duration::from_secs(5);

                    // 使用 timeout 包装 wait，避免无限等待
                    match tokio::time::timeout(timeout_duration, child.wait()).await {
                        Ok(Ok(status)) => {
                            info!(
                                "✅ Claude process exited gracefully with status: {} (project: {})",
                                status, self.inner.project_id
                            );
                        }
                        Ok(Err(e)) => {
                            warn!(
                                "⚠️ Failed to wait for Claude process (project: {}): {}",
                                self.inner.project_id, e
                            );
                        }
                        Err(_) => {
                            // 超时后强制 kill
                            warn!(
                                "⏰ Claude process didn't exit within {}s, force killing (project: {})",
                                timeout_duration.as_secs(),
                                self.inner.project_id
                            );

                            if let Err(e) = child.kill().await {
                                warn!(
                                    "⚠️ Failed to kill Claude process (project: {}): {}",
                                    self.inner.project_id, e
                                );
                            } else {
                                info!(
                                    "💀 Claude process force killed (project: {})",
                                    self.inner.project_id
                                );
                            }
                        }
                    }
                }
            }
        }

        self.inner.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// 发送取消信号（非阻塞）
    pub fn cancel(&self) {
        info!("Sending cancel signal to agent: {}", self.inner.project_id);
        self.inner.cancel_token.cancel();
    }

    /// 检查是否已停止
    pub fn is_stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    /// 获取取消令牌
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.inner.cancel_token
    }
}

impl Clone for AgentLifecycleGuard {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for AgentLifecycleGuard {
    fn drop(&mut self) {
        let strong_count = Arc::strong_count(&self.inner);
        let is_stopped = self.inner.stopped.load(Ordering::SeqCst);

        info!(
            "[Claude] AgentLifecycleGuard::drop 开始: project_id={}, strong_count={}, is_stopped={}",
            self.inner.project_id, strong_count, is_stopped
        );

        // 只有最后一个引用被drop时才执行清理
        if strong_count == 1 && !is_stopped {
            info!(
                "[Claude] AgentLifecycleGuard被drop，清理资源: {}",
                self.inner.project_id
            );

            // 发送取消信号
            self.inner.cancel_token.cancel();

            // 注意：API 密钥配置的清理由 agent_runner 层的 stop_agent 方法统一负责
            // 包括：
            // - shared_api_key_manager 中的配置
            // - project_uuid_map 中的映射
            //
            // 这样避免双重清理，确保资源只被清理一次

            // 同步清理关键资源
            match &self.inner.resources {
                AgentResources::Claude { child_process, .. } => {
                    info!(
                        "[Claude] 尝试获取 child_process 锁: {}",
                        self.inner.project_id
                    );
                    if let Ok(mut child_guard) = child_process.try_lock()
                        && let Some(mut child) = child_guard.take()
                    {
                        info!("[Claude] 开始 kill 子进程: {}", self.inner.project_id);
                        let _ = child.start_kill();
                        info!("[Claude] kill 子进程完成: {}", self.inner.project_id);
                    } else {
                        info!(
                            "[Claude] 无法获取 child_process 锁或子进程不存在: {}",
                            self.inner.project_id
                        );
                    }
                }
            }

            self.inner.stopped.store(true, Ordering::SeqCst);
        }

        info!(
            "[Claude] AgentLifecycleGuard::drop 完成: project_id={}",
            self.inner.project_id
        );
    }
}

// 为AgentLifecycleGuard实现AgentLifecycle trait
impl AgentLifecycle for AgentLifecycleGuard {
    fn graceful_stop(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move { AgentLifecycleGuard::graceful_stop(self).await })
    }

    fn cancel(&self) {
        AgentLifecycleGuard::cancel(self);
    }

    fn is_stopped(&self) -> bool {
        AgentLifecycleGuard::is_stopped(self)
    }

    fn cancellation_token(&self) -> &CancellationToken {
        AgentLifecycleGuard::cancellation_token(self)
    }
}

// 类型别名
pub type AgentStopGuard = AgentLifecycleGuard;
pub type AgentStopHandleArc = Arc<AgentLifecycleGuard>;

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::AgentLifecycle;
    use std::process::Stdio;

    /// 创建一个简单的测试进程（sleep 命令）用于生命周期测试
    async fn create_test_process() -> (tokio::process::Child, JoinHandle<()>) {
        let child = tokio::process::Command::new("sleep")
            .arg("60") // sleep 60 秒，测试会在此之前终止它
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("Failed to spawn test process");

        // 创建一个 dummy stderr 任务
        let stderr_task = tokio::spawn(async {
            // 简单的 dummy 任务
            tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
        });

        (child, stderr_task)
    }

    #[test]
    fn test_lifecycle_guard_debug() {
        // 使用 tokio runtime 创建 guard
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (child, stderr_task) = create_test_process().await;
            let cancel_token = CancellationToken::new();
            let session_id = sacp::schema::SessionId::new(Arc::from("test-session"));

            let guard = AgentLifecycleGuard::new_claude(
                "test-project".to_string(),
                session_id,
                child,
                stderr_task,
                cancel_token,
            );

            let debug_str = format!("{:?}", guard);
            assert!(debug_str.contains("AgentLifecycleGuard"));
            assert!(debug_str.contains("test-project"));
            assert!(debug_str.contains("stopped"));

            // 清理
            guard.cancel();
        });
    }

    #[test]
    fn test_lifecycle_guard_initial_state() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (child, stderr_task) = create_test_process().await;
            let cancel_token = CancellationToken::new();
            let session_id = sacp::schema::SessionId::new(Arc::from("test-session"));

            let guard = AgentLifecycleGuard::new_claude(
                "test-project".to_string(),
                session_id,
                child,
                stderr_task,
                cancel_token,
            );

            // 初始状态应该是未停止
            assert!(!guard.is_stopped());
            assert!(!guard.cancellation_token().is_cancelled());

            // 清理
            guard.cancel();
        });
    }

    #[test]
    fn test_lifecycle_guard_cancel() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (child, stderr_task) = create_test_process().await;
            let cancel_token = CancellationToken::new();
            let session_id = sacp::schema::SessionId::new(Arc::from("test-session"));

            let guard = AgentLifecycleGuard::new_claude(
                "test-project".to_string(),
                session_id,
                child,
                stderr_task,
                cancel_token,
            );

            // 取消前
            assert!(!guard.cancellation_token().is_cancelled());

            // 发送取消信号
            guard.cancel();

            // 取消后
            assert!(guard.cancellation_token().is_cancelled());
        });
    }

    #[test]
    fn test_lifecycle_guard_clone() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (child, stderr_task) = create_test_process().await;
            let cancel_token = CancellationToken::new();
            let session_id = sacp::schema::SessionId::new(Arc::from("test-session"));

            let guard1 = AgentLifecycleGuard::new_claude(
                "test-project".to_string(),
                session_id,
                child,
                stderr_task,
                cancel_token,
            );

            // Clone
            let guard2 = guard1.clone();

            // 两个 guard 应该共享相同的状态
            assert!(!guard1.is_stopped());
            assert!(!guard2.is_stopped());

            // 通过其中一个 guard 取消
            guard1.cancel();

            // 两者都应该看到取消状态
            assert!(guard1.cancellation_token().is_cancelled());
            assert!(guard2.cancellation_token().is_cancelled());
        });
    }

    #[tokio::test]
    async fn test_lifecycle_guard_graceful_stop() {
        let (child, stderr_task) = create_test_process().await;
        let cancel_token = CancellationToken::new();
        let session_id = sacp::schema::SessionId::new(Arc::from("test-session"));

        let guard = AgentLifecycleGuard::new_claude(
            "test-project".to_string(),
            session_id,
            child,
            stderr_task,
            cancel_token,
        );

        // 执行优雅停止
        let result = guard.graceful_stop().await;
        assert!(result.is_ok());

        // 停止后应该标记为已停止
        assert!(guard.is_stopped());
        assert!(guard.cancellation_token().is_cancelled());
    }

    #[tokio::test]
    async fn test_lifecycle_guard_graceful_stop_idempotent() {
        let (child, stderr_task) = create_test_process().await;
        let cancel_token = CancellationToken::new();
        let session_id = sacp::schema::SessionId::new(Arc::from("test-session"));

        let guard = AgentLifecycleGuard::new_claude(
            "test-project".to_string(),
            session_id,
            child,
            stderr_task,
            cancel_token,
        );

        // 第一次停止
        let result1 = guard.graceful_stop().await;
        assert!(result1.is_ok());
        assert!(guard.is_stopped());

        // 第二次停止应该是幂等的（不会报错）
        let result2 = guard.graceful_stop().await;
        assert!(result2.is_ok());
        assert!(guard.is_stopped());
    }

    #[tokio::test]
    async fn test_agent_lifecycle_trait() {
        let (child, stderr_task) = create_test_process().await;
        let cancel_token = CancellationToken::new();
        let session_id = sacp::schema::SessionId::new(Arc::from("test-session"));

        let guard = AgentLifecycleGuard::new_claude(
            "test-project".to_string(),
            session_id,
            child,
            stderr_task,
            cancel_token,
        );

        // 通过 trait 方法调用
        let lifecycle: &dyn AgentLifecycle = &guard;

        assert!(!lifecycle.is_stopped());
        assert!(!lifecycle.cancellation_token().is_cancelled());

        lifecycle.cancel();
        assert!(lifecycle.cancellation_token().is_cancelled());

        // 停止
        let result = lifecycle.graceful_stop().await;
        assert!(result.is_ok());
        assert!(lifecycle.is_stopped());
    }

    #[test]
    fn test_type_aliases() {
        // 验证类型别名正确
        fn _accepts_stop_guard(_: AgentStopGuard) {}
        fn _accepts_arc(_: AgentStopHandleArc) {}

        // 如果编译通过，说明类型别名正确
    }
}
