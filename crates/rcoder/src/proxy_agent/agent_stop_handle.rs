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

use crate::model::AgentType;
use agent_client_protocol::{ClientSideConnection, SessionId};

/// Agent生命周期守卫
/// 
/// 遵循RAII原则，当守卫被drop时自动清理agent资源
pub struct AgentLifecycleGuard {
    inner: Arc<AgentLifecycleInner>,
}

struct AgentLifecycleInner {
    agent_type: AgentType,
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
    Codex {
        client_conn: Arc<ClientSideConnection>,
        io_tasks: Arc<Mutex<Vec<JoinHandle<Result<(), anyhow::Error>>>>>,
        channel_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
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
            agent_type: AgentType::Claude,
            project_id,
            session_id,
            cancel_token,
            resources,
            stopped: AtomicBool::new(false),
        });

        Self { inner }
    }

    /// 为Codex Agent创建生命周期守卫
    pub fn new_codex(
        project_id: String,
        session_id: SessionId,
        client_conn: Arc<ClientSideConnection>,
        io_tasks: Vec<JoinHandle<Result<(), anyhow::Error>>>,
        channel_tasks: Vec<JoinHandle<()>>,
        cancel_token: CancellationToken,
    ) -> Self {
        let resources = AgentResources::Codex {
            client_conn,
            io_tasks: Arc::new(Mutex::new(io_tasks)),
            channel_tasks: Arc::new(Mutex::new(channel_tasks)),
        };

        let inner = Arc::new(AgentLifecycleInner {
            agent_type: AgentType::Codex,
            project_id,
            session_id,
            cancel_token,
            resources,
            stopped: AtomicBool::new(false),
        });

        Self { inner }
    }

    /// 优雅停止agent
    pub async fn graceful_stop(&self) -> Result<()> {
        if self.inner.stopped.swap(true, Ordering::SeqCst) {
            return Ok(()); // 已经停止
        }

        let agent_name = match self.inner.agent_type {
            AgentType::Claude => "Claude",
            AgentType::Codex => "Codex",
        };

        info!(
            "[{}] 开始优雅停止agent: {} (session: {})",
            agent_name,
            self.inner.project_id,
            self.inner.session_id.0
        );

        // 1. 发送取消信号
        self.inner.cancel_token.cancel();

        // 2. 等待任务自然退出
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // 3. 强制清理资源
        self.force_cleanup().await?;

        info!(
            "[{}] agent优雅停止完成: {}",
            agent_name,
            self.inner.project_id
        );

        Ok(())
    }

    /// 强制清理资源
    async fn force_cleanup(&self) -> Result<()> {
        match &self.inner.resources {
            AgentResources::Claude { child_process, stderr_task } => {
                // 停止stderr任务
                if let Some(task) = stderr_task.lock().await.take() {
                    task.abort();
                }

                // 终止子进程
                if let Some(mut child) = child_process.lock().await.take() {
                    if let Err(e) = child.kill().await {
                        warn!("终止Claude子进程失败: {}", e);
                    }
                }
            }
            AgentResources::Codex { io_tasks, channel_tasks, .. } => {
                // 取消所有任务
                for task in io_tasks.lock().await.drain(..) {
                    task.abort();
                }
                for task in channel_tasks.lock().await.drain(..) {
                    task.abort();
                }
            }
        }
        Ok(())
    }

    /// 发送取消信号（非阻塞）
    pub fn cancel(&self) {
        if !self.inner.cancel_token.is_cancelled() {
            let agent_name = match self.inner.agent_type {
                AgentType::Claude => "Claude",
                AgentType::Codex => "Codex",
            };
            info!(
                "[{}] 发送取消信号: {} (session: {})",
                agent_name,
                self.inner.project_id,
                self.inner.session_id.0
            );
            self.inner.cancel_token.cancel();
        }
    }

    /// 异步停止
    pub async fn stop_async(&self) -> Result<()> {
        self.graceful_stop().await
    }

    /// 检查是否已停止
    pub fn is_stopped(&self) -> bool {
        self.inner.stopped.load(Ordering::SeqCst)
    }

    /// 获取取消令牌
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.inner.cancel_token
    }

    /// 获取agent类型
    pub fn agent_type(&self) -> AgentType {
        self.inner.agent_type
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
        if Arc::strong_count(&self.inner) == 1 {
            if !self.inner.stopped.load(Ordering::SeqCst) {
                let agent_name = match self.inner.agent_type {
                    AgentType::Claude => "Claude",
                    AgentType::Codex => "Codex",
                };
                info!(
                    "[{}] AgentLifecycleGuard被drop，清理资源: {}",
                    agent_name,
                    self.inner.project_id
                );

                // 发送取消信号
                self.inner.cancel_token.cancel();

                // 同步清理关键资源
                match &self.inner.resources {
                    AgentResources::Claude { child_process, .. } => {
                        if let Ok(mut child_guard) = child_process.try_lock() {
                            if let Some(mut child) = child_guard.take() {
                                let _ = child.start_kill();
                            }
                        }
                    }
                    AgentResources::Codex { io_tasks, channel_tasks, .. } => {
                        if let Ok(mut tasks) = io_tasks.try_lock() {
                            for task in tasks.drain(..) {
                                task.abort();
                            }
                        }
                        if let Ok(mut tasks) = channel_tasks.try_lock() {
                            for task in tasks.drain(..) {
                                task.abort();
                            }
                        }
                    }
                }

                self.inner.stopped.store(true, Ordering::SeqCst);
            }
        }
    }
}

// 类型别名
pub type AgentStopGuard = AgentLifecycleGuard;
pub type AgentStopHandleArc = Arc<AgentLifecycleGuard>;