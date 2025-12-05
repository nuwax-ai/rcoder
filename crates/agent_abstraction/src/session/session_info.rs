//! Session 信息结构体
//!
//! 存储 ACP 会话的完整信息

use std::sync::Arc;

use agent_client_protocol::{PromptRequest, SessionId};
use chrono::{DateTime, Utc};
use shared_types::{
    AgentLifecycle, AgentStatus, CancelNotificationRequestWrapper, ModelProviderConfig,
};
use tokio::sync::mpsc;

/// ACP 会话信息
///
/// 存储与单个 Agent 会话相关的所有信息
#[derive(Clone)]
pub struct SessionInfo {
    /// 项目 ID（业务主键）
    pub project_id: String,
    /// ACP 会话 ID
    pub session_id: SessionId,
    /// 发送 Prompt 消息的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// 发送取消请求的通道（使用统一的新类型）
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequestWrapper>,
    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,
    /// 当前活跃的请求 ID
    pub request_id: Option<String>,
    /// Agent 服务状态
    pub status: AgentStatus,
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// Agent 生命周期管理句柄
    pub lifecycle_handle: Option<Arc<dyn AgentLifecycle>>,
}

impl SessionInfo {
    /// 创建新的会话信息
    pub fn new(
        project_id: String,
        session_id: SessionId,
        prompt_tx: mpsc::UnboundedSender<PromptRequest>,
        cancel_tx: mpsc::UnboundedSender<CancelNotificationRequestWrapper>,
        model_provider: Option<ModelProviderConfig>,
        lifecycle_handle: Option<Arc<dyn AgentLifecycle>>,
    ) -> Self {
        let now = Utc::now();
        Self {
            project_id,
            session_id,
            prompt_tx,
            cancel_tx,
            model_provider,
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: now,
            created_at: now,
            lifecycle_handle,
        }
    }

    /// 更新最后活动时间
    pub fn touch(&mut self) {
        self.last_activity = Utc::now();
    }

    /// 设置请求 ID
    pub fn set_request_id(&mut self, request_id: Option<String>) {
        self.request_id = request_id;
    }

    /// 设置状态
    pub fn set_status(&mut self, status: AgentStatus) {
        self.status = status;
    }

    /// 检查会话是否已过期
    ///
    /// # Arguments
    /// * `timeout_secs` - 超时时间（秒）
    pub fn is_expired(&self, timeout_secs: i64) -> bool {
        let duration = Utc::now() - self.last_activity;
        duration.num_seconds() > timeout_secs
    }

    /// 检查模型配置是否与当前不同
    pub fn is_model_config_changed(&self, new_config: &Option<ModelProviderConfig>) -> bool {
        match (&self.model_provider, new_config) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(existing), Some(new)) => existing.id != new.id,
        }
    }
}

impl std::fmt::Debug for SessionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionInfo")
            .field("project_id", &self.project_id)
            .field("session_id", &self.session_id.0)
            .field("status", &self.status)
            .field("last_activity", &self.last_activity)
            .field("created_at", &self.created_at)
            .field("has_lifecycle_handle", &self.lifecycle_handle.is_some())
            .finish()
    }
}
