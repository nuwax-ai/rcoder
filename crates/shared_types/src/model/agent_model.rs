//! Agent 相关的核心结构体 - rcoder 和 agent_runner 共用

use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{ModelProviderConfig, ModelProviderSafeInfo};
use agent_client_protocol::{CancelNotification, PromptRequest, SessionId};
use chrono::{DateTime, Utc};
use tokio::sync::{mpsc, oneshot};
use utoipa::ToSchema;

/// 取消通知请求
pub struct CancelNotificationRequest {
    pub cancel_notification: CancelNotification,
    pub tx: oneshot::Sender<CancelNotificationResponse>,
}

/// 取消通知响应
#[derive(Debug)]
pub struct CancelNotificationResponse {
    pub success: bool,
    pub message: Option<String>,
}

/// Agent 服务状态
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, ToSchema)]
pub enum AgentStatus {
    /// 活跃状态 - 正在处理请求
    Active,
    /// 空闲状态 - 等待新请求
    Idle,
    /// 正在终止
    Terminating,
}

/// 项目id与 Agent 服务池，一个项目对应一个 Agent 服务
///
/// Clone trait 是必需的，因为 DashMap::insert() 要求值类型实现 Clone
#[derive(Clone)]
pub struct ProjectAndAgentInfo {
    /// 项目ID
    pub project_id: String,
    /// 会话ID，agent 服务启动时会创建一个会话ID
    pub session_id: SessionId,
    /// 用于发送 Prompt 的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// 用于发送取消通知的通道
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequest>,
    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,
    /// 当前活跃的请求ID，用于标识用户请求
    pub request_id: Option<String>,
    /// Agent 服务状态
    pub status: AgentStatus,
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// Agent生命周期管理句柄
    pub stop_handle: Option<Arc<dyn AgentLifecycle>>,
}

/// Agent 状态查询响应
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct AgentStatusResponse {
    /// 项目ID
    #[schema(example = "test_project")]
    pub project_id: String,
    /// Agent 是否存活
    #[schema(example = true)]
    pub is_alive: bool,
    /// 会话ID（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "session123")]
    pub session_id: Option<String>,
    /// Agent 服务状态（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentStatus>,
    /// 最后活动时间（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "2024-01-01T12:00:00Z")]
    pub last_activity: Option<DateTime<Utc>>,
    /// 创建时间（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "2024-01-01T10:00:00Z")]
    pub created_at: Option<DateTime<Utc>>,
    /// 模型提供商安全信息（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<ModelProviderSafeInfo>,
}

/// Agent生命周期守卫
///
/// 遵循RAII原则，当守卫被drop时自动清理agent资源
pub struct AgentLifecycleGuard {
    inner: Arc<AgentLifecycleInner>,
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
    pub async fn graceful_stop(&self) -> Result<()> {
        if self.inner.stopped.swap(true, Ordering::SeqCst) {
            return Ok(()); // 已经停止
        }

        info!(
            "[Claude] 开始优雅停止agent: {} (session: {})",
            self.inner.project_id, self.inner.session_id.0
        );

        // 1. 发送取消信号
        self.inner.cancel_token.cancel();

        // 2. 等待任务自然退出
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // 3. 强制清理资源
        self.force_cleanup().await?;

        info!(
            "[Claude] agent优雅停止完成: {}",
            self.inner.project_id
        );

        Ok(())
    }

    /// 强制清理资源
    async fn force_cleanup(&self) -> Result<()> {
        match &self.inner.resources {
            AgentResources::Claude {
                child_process,
                stderr_task,
            } => {
                // 停止stderr任务
                if let Some(task) = stderr_task.lock().await.take() {
                    task.abort();
                }

                // 终止子进程
                if let Some(mut child) = child_process.lock().await.take()
                    && let Err(e) = child.kill().await
                {
                    warn!("终止Claude子进程失败: {}", e);
                }
            }
        }
        Ok(())
    }

    /// 发送取消信号（非阻塞）
    pub fn cancel(&self) {
        if !self.inner.cancel_token.is_cancelled() {
            info!(
                "[Claude] 发送取消信号: {} (session: {})",
                self.inner.project_id, self.inner.session_id.0
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

/// Agent生命周期trait
///
/// 定义了Agent生命周期管理的基本接口
pub trait AgentLifecycle: Send + Sync + 'static {
    /// 优雅停止Agent
    fn graceful_stop(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;

    /// 发送取消信号（非阻塞）
    fn cancel(&self);

    /// 检查是否已停止
    fn is_stopped(&self) -> bool;

    /// 获取取消令牌
    fn cancellation_token(&self) -> &CancellationToken;
}

/// Agent停止句柄
///
/// 包装AgentLifecycleGuard，提供统一的trait接口
pub struct AgentStopHandle {
    inner: Arc<dyn AgentLifecycle>,
}

impl AgentStopHandle {
    /// 创建新的AgentStopHandle
    pub fn new(inner: Arc<dyn AgentLifecycle>) -> Self {
        Self { inner }
    }

    /// 获取内部引用
    pub fn inner(&self) -> &Arc<dyn AgentLifecycle> {
        &self.inner
    }
}

impl std::ops::Deref for AgentStopHandle {
    type Target = dyn AgentLifecycle;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref()
    }
}

// 为AgentLifecycleGuard实现AgentLifecycle trait
impl AgentLifecycle for AgentLifecycleGuard {
    fn graceful_stop(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move { self.graceful_stop().await })
    }

    fn cancel(&self) {
        self.cancel()
    }

    fn is_stopped(&self) -> bool {
        self.is_stopped()
    }

    fn cancellation_token(&self) -> &CancellationToken {
        self.cancellation_token()
    }
}

// 类型别名
pub type AgentStopGuard = AgentLifecycleGuard;
pub type AgentStopHandleArc = Arc<AgentLifecycleGuard>;
