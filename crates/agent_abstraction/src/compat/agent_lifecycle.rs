//! Agent生命周期管理
//!
//! 基于RAII原则的简洁生命周期管理设计

use anyhow::Result;
use dashmap::DashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use agent_client_protocol::SessionId;
use shared_types::{AgentLifecycle, ModelProviderConfig};

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
    /// 🔥 共享的 API 密钥管理器引用（用于自动清理）
    shared_api_key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
    /// 🔥 project_id -> service_uuid 映射（用于清理时查找 UUID）
    project_uuid_map: Option<Arc<DashMap<String, String>>>,
    /// 🔥 关联的 service_uuid（用于清理时定位配置）
    service_uuid: Option<String>,
}

/// Agent资源管理枚举
enum AgentResources {
    Claude {
        child_process: Arc<Mutex<Option<tokio::process::Child>>>,
        stderr_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    },
}

impl AgentLifecycleGuard {
    /// 为Claude Agent创建生命周期守卫（兼容旧代码，默认无密钥管理器）
    pub fn new_claude(
        project_id: String,
        session_id: SessionId,
        child_process: tokio::process::Child,
        stderr_task: JoinHandle<()>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self::new_claude_with_key_manager(
            project_id,
            session_id,
            child_process,
            stderr_task,
            cancel_token,
            None,  // 默认无密钥管理器
            None,  // 默认无 project_uuid_map
            None,  // 默认无 service_uuid
        )
    }

    /// 🔥 新增：带密钥管理器的构造函数
    ///
    /// 创建生命周期守卫时传入共享的 API 密钥管理器和 service_uuid，
    /// 当 Agent 停止时（Drop）会自动清理对应的 API 密钥配置。
    ///
    /// # 参数
    ///
    /// * `shared_api_key_manager` - 共享的 DashMap，用于清理 API 密钥配置
    /// * `project_uuid_map` - project_id -> service_uuid 映射，用于查找 UUID
    /// * `service_uuid` - 与此 Agent 关联的 service UUID
    pub fn new_claude_with_key_manager(
        project_id: String,
        session_id: SessionId,
        child_process: tokio::process::Child,
        stderr_task: JoinHandle<()>,
        cancel_token: CancellationToken,
        shared_api_key_manager: Option<Arc<DashMap<String, ModelProviderConfig>>>,
        project_uuid_map: Option<Arc<DashMap<String, String>>>,
        service_uuid: Option<String>,
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
            shared_api_key_manager,
            project_uuid_map,
            service_uuid,
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
