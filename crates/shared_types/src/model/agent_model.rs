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
// SACP 类型导入（替代 agent_client_protocol）
use chrono::{DateTime, Utc};
use sacp::schema::{CancelNotification, PromptRequest, SessionId};
use tokio::sync::{mpsc, oneshot};
use utoipa::ToSchema;

// ============================================================================
// SessionEntry Trait - 会话条目抽象
// ============================================================================

/// 会话条目 trait
///
/// 抽象会话条目的数据访问接口，允许不同的结构体实现此 trait。
/// 用于消除 `SessionInfo` 和 `ProjectAndAgentInfo` 的重复。
pub trait SessionEntry: Clone + Send + Sync + 'static {
    /// 获取项目 ID
    fn project_id(&self) -> &str;

    /// 获取会话 ID
    fn session_id(&self) -> &SessionId;

    /// 获取 Prompt 发送通道（有界通道）
    fn prompt_tx(&self) -> &mpsc::Sender<PromptRequest>;

    /// 获取取消通知发送通道（有界通道）
    fn cancel_tx(&self) -> &mpsc::Sender<CancelNotificationRequestWrapper>;

    /// 获取模型配置
    fn model_provider(&self) -> Option<&ModelProviderConfig>;

    /// 获取生命周期管理句柄
    fn lifecycle_handle(&self) -> Option<&Arc<dyn AgentLifecycle>>;

    /// 获取 Agent 状态
    fn status(&self) -> &AgentStatus;

    /// 检查 channel 是否已关闭（Agent 进程已退出）
    fn is_channel_closed(&self) -> bool;

    /// 检查模型配置是否与给定配置不同
    fn is_model_config_changed(&self, new_config: &Option<ModelProviderConfig>) -> bool;
}

/// 取消操作结果（新类型）
///
/// 用于统一取消操作的返回结果
#[derive(Debug, Clone)]
pub enum CancelResult {
    /// 取消成功
    Success,
    /// 取消失败
    Failed(String),
    /// 取消超时
    Timeout,
}

impl CancelResult {
    /// 是否成功
    pub fn is_success(&self) -> bool {
        matches!(self, CancelResult::Success)
    }

    /// 获取错误信息
    pub fn error_message(&self) -> Option<&str> {
        match self {
            CancelResult::Failed(msg) => Some(msg),
            CancelResult::Timeout => Some("Cancel operation timed out"),
            CancelResult::Success => None,
        }
    }
}

/// 取消通知请求包装器（新类型）
///
/// 直接包含 CancelNotification 和结果回调通道，
/// 替代旧的 CancelNotificationRequest
pub struct CancelNotificationRequestWrapper {
    /// 取消通知
    pub cancel_notification: CancelNotification,
    /// 结果回调通道
    pub result_tx: oneshot::Sender<CancelResult>,
}

impl std::fmt::Debug for CancelNotificationRequestWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelNotificationRequestWrapper")
            .field("cancel_notification", &"<CancelNotification>")
            .field("result_tx", &"<oneshot::Sender>")
            .finish()
    }
}

/// 取消通知请求（旧类型，保留兼容性）
#[deprecated(note = "Use CancelNotificationRequestWrapper instead")]
#[allow(dead_code)]
pub struct CancelNotificationRequest {
    pub cancel_notification: CancelNotification,
    pub tx: oneshot::Sender<CancelNotificationResponse>,
}

/// 取消通知响应（旧类型，保留用于 HTTP 响应）
#[derive(Debug)]
pub struct CancelNotificationResponse {
    pub success: bool,
    pub message: Option<String>,
}

impl From<CancelResult> for CancelNotificationResponse {
    fn from(result: CancelResult) -> Self {
        match result {
            CancelResult::Success => CancelNotificationResponse {
                success: true,
                message: None,
            },
            CancelResult::Failed(msg) => CancelNotificationResponse {
                success: false,
                message: Some(msg),
            },
            CancelResult::Timeout => CancelNotificationResponse {
                success: false,
                message: Some("取消请求超时".to_string()),
            },
        }
    }
}

/// Agent 服务状态
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, ToSchema)]
pub enum AgentStatus {
    /// 等待处理 - 任务已提交到队列，等待 Worker 开始处理
    Pending,
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
    /// 用于发送 Prompt 的通道（有界通道，提供背压保护）
    pub prompt_tx: mpsc::Sender<PromptRequest>,
    /// 用于发送取消通知的通道（有界通道，提供背压保护）
    pub cancel_tx: mpsc::Sender<CancelNotificationRequestWrapper>,
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

// ============================================================================
// ProjectAndAgentInfo 实现 SessionEntry trait
// ============================================================================

impl SessionEntry for ProjectAndAgentInfo {
    fn project_id(&self) -> &str {
        &self.project_id
    }

    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    fn prompt_tx(&self) -> &mpsc::Sender<PromptRequest> {
        &self.prompt_tx
    }

    fn cancel_tx(&self) -> &mpsc::Sender<CancelNotificationRequestWrapper> {
        &self.cancel_tx
    }

    fn model_provider(&self) -> Option<&ModelProviderConfig> {
        self.model_provider.as_ref()
    }

    fn lifecycle_handle(&self) -> Option<&Arc<dyn AgentLifecycle>> {
        self.stop_handle.as_ref()
    }

    fn status(&self) -> &AgentStatus {
        &self.status
    }

    fn is_channel_closed(&self) -> bool {
        self.prompt_tx.is_closed()
    }

    fn is_model_config_changed(&self, new_config: &Option<ModelProviderConfig>) -> bool {
        match (&self.model_provider, new_config) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(existing), Some(new)) => existing.id != new.id,
        }
    }
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
            "[Claude] agentgracefulstoppedcompleted: {}",
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
                    warn!(" message Claudechild processfailed: {}", e);
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
                        && let Err(e) = child.start_kill()
                    {
                        tracing::warn!(
                            "⚠️ [AGENT] start_kill 失败: project_id={}, error={}",
                            self.inner.project_id,
                            e
                        );
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
