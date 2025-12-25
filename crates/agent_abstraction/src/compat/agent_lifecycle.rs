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

use agent_client_protocol::SessionId;
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
        stderr_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    },
}

impl AgentLifecycleGuard {
    /// 为Claude Agent创建生命周期守卫
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
        // 只有最后一个引用被drop时才执行清理
        if Arc::strong_count(&self.inner) == 1 && !self.inner.stopped.load(Ordering::SeqCst) {
            info!(
                "[Claude] AgentLifecycleGuard被drop，清理资源: {}",
                self.inner.project_id
            );

            // 发送取消信号
            self.inner.cancel_token.cancel();

            // 同步清理关键资源
            match &self.inner.resources {
                AgentResources::Claude { child_process, .. } => {
                    if let Ok(mut child_guard) = child_process.try_lock()
                        && let Some(mut child) = child_guard.take()
                    {
                        let _ = child.start_kill();
                    }
                }
            }

            self.inner.stopped.store(true, Ordering::SeqCst);
        }
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
