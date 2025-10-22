//! 会话缓存管理
//!
//! 提供全局的会话缓存和项目管理功能

use std::sync::{LazyLock, Arc};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tracing::debug;

/// 全局会话缓存
pub static SESSION_CACHE: LazyLock<DashMap<String, Arc<SessionData>>> =
    LazyLock::new(|| DashMap::new());

/// 项目到会话的映射
pub static PROJECT_SESSION_MAP: LazyLock<DashMap<String, String>> =
    LazyLock::new(|| DashMap::new());

/// 会话请求数据
#[derive(Debug, Clone)]
pub struct SessionData {
    /// 会话ID
    pub session_id: String,
    /// 项目ID
    pub project_id: Option<String>,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
    /// 消息通道
    pub message_tx: Option<mpsc::UnboundedSender<UnifiedSessionMessage>>,
}

/// 统一会话消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedSessionMessage {
    /// 消息类型
    pub message_type: SessionMessageType,
    /// 消息内容
    pub content: serde_json::Value,
    /// 时间戳
    pub timestamp: DateTime<Utc>,
}

/// 会话消息类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionMessageType {
    /// 进度更新
    Progress,
    /// 任务完成
    Complete,
    /// 错误消息
    Error,
    /// 状态更新
    Status,
}

impl SessionData {
    /// 创建新的会话数据
    pub fn new(session_id: String, project_id: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            session_id,
            project_id,
            created_at: now,
            last_activity: now,
            message_tx: None,
        }
    }

    /// 获取消息数量
    pub async fn message_count(&self) -> usize {
        // 简化实现，实际应该从消息队列获取
        0
    }

    /// 关闭当前连接
    pub fn close_current_connection(&self) {
        // 简化实现
        debug!("关闭会话连接: {}", self.session_id);
    }
}

/// 确保项目的会话映射存在
///
/// # Arguments
/// * `project_id` - 项目ID
/// * `session_id` - 会话ID
///
/// # Returns
/// 返回清理的旧会话数量
pub async fn ensure_project_session(project_id: &str, session_id: &str) -> usize {
    let mut cleared_count = 0;

    // 检查是否已有会话映射
    if let Some(old_session_id) = PROJECT_SESSION_MAP.get(project_id) {
        if old_session_id.as_str() != session_id {
            // 清理旧会话
            if SESSION_CACHE.remove(old_session_id.as_str()).is_some() {
                cleared_count = 1;
                debug!("清理旧会话映射: project_id={}, old_session_id={:?}", project_id, old_session_id);
            }
        }
    }

    // 设置新的会话映射
    PROJECT_SESSION_MAP.insert(project_id.to_string(), session_id.to_string());

    cleared_count
}

/// SESSION_REQUEST_CONTEXT - 请求上下文映射
pub static SESSION_REQUEST_CONTEXT: LazyLock<DashMap<String, String>> =
    LazyLock::new(|| DashMap::new());