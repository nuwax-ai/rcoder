//! Proxy Agent 模块
//!
//! 使用 SACP 协议（symposium-acp），完全移除旧版 agent-client-protocol 依赖。

pub mod cleanup_task;
mod sacp_agent;

use dashmap::DashMap;
use std::sync::LazyLock;

// SACP 版本的导出
pub use sacp_agent::{SacpAgentRequest, sacp_agent_worker};

/// 会话级别的 request_id 上下文映射（project_id -> request_id）
/// 用于在 session_notification 回调中获取当前请求的 request_id
/// 避免使用 PROJECT_AND_AGENT_INFO_MAP 导致的锁竞争问题
/// 注意：使用 project_id 而非 session_id，确保同一项目的多次请求能自动覆盖为最新值
pub static SESSION_REQUEST_CONTEXT: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);
