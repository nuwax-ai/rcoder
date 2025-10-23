mod acp_agent;
pub mod cleanup_task;
pub mod container_monitor;
pub mod container_service;
pub mod docker_container_agent;
pub mod network_management;
pub mod port_manager;

pub use acp_agent::PROJECT_AND_AGENT_INFO_MAP;
use dashmap::DashMap;
use std::sync::LazyLock;
/// 会话级别的 request_id 上下文映射（project_id -> request_id）
/// 用于在 session_notification 回调中获取当前请求的 request_id
/// 避免使用 PROJECT_AND_AGENT_INFO_MAP 导致的锁竞争问题
/// 注意：使用 project_id 而非 session_id，确保同一项目的多次请求能自动覆盖为最新值
pub static SESSION_REQUEST_CONTEXT: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);
