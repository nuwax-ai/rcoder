use std::collections::HashMap;
use agent_client_protocol::{SessionId, CancelNotification, PromptRequest};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use super::{ModelProviderConfig, ModelProviderSafeInfo, AgentType};
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