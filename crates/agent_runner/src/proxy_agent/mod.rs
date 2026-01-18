mod acp_agent;
pub mod cleanup_task;

use crate::CancelNotificationRequestWrapper;
// 导出 agent_worker 相关类型和函数
// AgentRequest 是 SACP 版本的新类型，LocalSetAgentRequest 是向后兼容别名
#[allow(deprecated)]
pub use acp_agent::{AgentRequest, LocalSetAgentRequest, agent_worker_with_heartbeat};
use shared_types::AgentLifecycleGuard;
// SACP 类型导入
use sacp::schema::{PromptRequest, SessionId};
use dashmap::DashMap;
use std::sync::{Arc, LazyLock};
use tokio::sync::mpsc;

/// 会话级别的 request_id 上下文映射（project_id -> request_id）
/// 用于在 session_notification 回调中获取当前请求的 request_id
/// 避免使用 PROJECT_AND_AGENT_INFO_MAP 导致的锁竞争问题
/// 注意：使用 project_id 而非 session_id，确保同一项目的多次请求能自动覆盖为最新值
pub static SESSION_REQUEST_CONTEXT: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);

/// ACP协议的连接信息
pub struct AcpConnectionInfo {
    /// 会话ID
    pub session_id: SessionId,
    /// 用于发送 Prompt 的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// 用于发送取消通知的通道（使用新类型）
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequestWrapper>,
    /// Agent停止句柄（将被包装为守卫并放入 ProjectAndAgentInfo）
    pub stop_handle: Option<Arc<AgentLifecycleGuard>>,
}
