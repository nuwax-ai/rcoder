mod acp_agent;
pub mod agent_stop_handle;
pub mod cleanup_task;
pub mod docker_agent;
pub mod docker_container_agent;
pub mod port_manager;
pub mod network_management;
pub mod container_monitor;
pub mod container_service;

use crate::CancelNotificationRequest;
use crate::{
    AgentSessionUpdate, AgentType, ProjectAndAgentInfo, SessionNotify,
};
pub use acp_agent::{LocalSetAgentRequest, PROJECT_AND_AGENT_INFO_MAP};
use agent_client_protocol::{Client, PermissionOptionKind, PromptRequest, SessionId};
use dashmap::DashMap;
use std::sync::LazyLock;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::proxy_agent::agent_stop_handle::AgentStopHandleArc;

/// 会话级别的 request_id 上下文映射（project_id -> request_id）
/// 用于在 session_notification 回调中获取当前请求的 request_id
/// 避免使用 PROJECT_AND_AGENT_INFO_MAP 导致的锁竞争问题
/// 注意：使用 project_id 而非 session_id，确保同一项目的多次请求能自动覆盖为最新值
pub static SESSION_REQUEST_CONTEXT: LazyLock<DashMap<String, String>> =
    LazyLock::new(DashMap::new);

/// ACP协议的连接信息
pub struct AcpConnectionInfo {
    /// 会话ID
    pub session_id: SessionId,
    /// 用于发送 Prompt 的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// 用于发送取消通知的通道
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequest>,
    /// Agent停止句柄（将被包装为守卫并放入 ProjectAndAgentInfo）
    pub stop_handle: Option<AgentStopHandleArc>,
}

