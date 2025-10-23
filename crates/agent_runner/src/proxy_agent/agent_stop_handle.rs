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
use shared_types::{AgentLifecycle, AgentType as SharedAgentType};

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
    /// Codex 子进程模式（与 Claude 类似）
    CodexSubProcess {
        child_process: Arc<Mutex<Option<tokio::process::Child>>>,
        stderr_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    },
    /// Codex 嵌入式模式（已废弃，官方不支持）
    #[allow(dead_code)]
    CodexEmbedded {
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

    /// 为Codex Agent创建生命周期守卫（子进程模式）
    pub fn new_codex(
        project_id: String,
        session_id: SessionId,
        child_process: tokio::process::Child,
        stderr_task: JoinHandle<()>,
        cancel_token: CancellationToken,
    ) -> Self {
        let resources = AgentResources::CodexSubProcess {
            child_process: Arc::new(Mutex::new(Some(child_process))),
            stderr_task: Arc::new(Mutex::new(Some(stderr_task))),
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

    /// 为Codex Agent创建生命周期守卫（嵌入式模式，已废弃）
    #[allow(dead_code)]
    pub fn new_codex_embedded(
        project_id: String,
        session_id: SessionId,
        client_conn: Arc<ClientSideConnection>,
        io_tasks: Vec<JoinHandle<Result<(), anyhow::Error>>>,
        channel_tasks: Vec<JoinHandle<()>>,
        cancel_token: CancellationToken,
    ) -> Self {
        let resources = AgentResources::CodexEmbedded {
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
        if self.inner.stopped.load(Ordering::SeqCst) {
            info!("Agent already stopped, skipping graceful stop");
            return Ok(());
        }

        info!(
            "Gracefully stopping {} agent for project: {}",
            match self.inner.agent_type {
                AgentType::Claude => "Claude",
                AgentType::Codex => "Codex",
            },
            self.inner.project_id
        );

        // 发送取消信号
        self.inner.cancel_token.cancel();

        // 根据资源类型执行相应的清理操作
        match &self.inner.resources {
            AgentResources::Claude { child_process, .. } => {
                let mut child_guard = child_process.lock().await;
                if let Some(mut child) = child_guard.take() {
                    info!("Stopping Claude child process");
                    match child.wait().await {
                        Ok(status) => info!("Claude process exited with status: {}", status),
                        Err(e) => warn!("Failed to wait for Claude process: {}", e),
                    }
                }
            }
            AgentResources::CodexSubProcess { child_process, .. } => {
                let mut child_guard = child_process.lock().await;
                if let Some(mut child) = child_guard.take() {
                    info!("Stopping Codex child process");
                    match child.wait().await {
                        Ok(status) => info!("Codex process exited with status: {}", status),
                        Err(e) => warn!("Failed to wait for Codex process: {}", e),
                    }
                }
            }
            AgentResources::CodexEmbedded {
                io_tasks,
                channel_tasks,
                ..
            } => {
                // 取消所有IO任务
                let mut tasks = io_tasks.lock().await;
                for task in tasks.drain(..) {
                    task.abort();
                }
                // 取消所有通道任务
                let mut tasks = channel_tasks.lock().await;
                for task in tasks.drain(..) {
                    task.abort();
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
        if Arc::strong_count(&self.inner) == 1 && !self.inner.stopped.load(Ordering::SeqCst) {
            let agent_name = match self.inner.agent_type {
                AgentType::Claude => "Claude",
                AgentType::Codex => "Codex",
            };
            info!(
                "[{}] AgentLifecycleGuard被drop，清理资源: {}",
                agent_name, self.inner.project_id
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
                AgentResources::CodexSubProcess { child_process, .. } => {
                    if let Ok(mut child_guard) = child_process.try_lock()
                        && let Some(mut child) = child_guard.take()
                    {
                        let _ = child.start_kill();
                    }
                }
                AgentResources::CodexEmbedded {
                    io_tasks,
                    channel_tasks,
                    ..
                } => {
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

// 为AgentLifecycleGuard实现AgentLifecycle trait
impl AgentLifecycle for AgentLifecycleGuard {
    fn graceful_stop(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            AgentLifecycleGuard::graceful_stop(self).await
        })
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

    fn agent_type(&self) -> SharedAgentType {
        match AgentLifecycleGuard::agent_type(self) {
            AgentType::Claude => SharedAgentType::Claude,
            AgentType::Codex => SharedAgentType::Codex,
        }
    }
}

// 类型别名
pub type AgentStopGuard = AgentLifecycleGuard;
pub type AgentStopHandleArc = Arc<AgentLifecycleGuard>;

