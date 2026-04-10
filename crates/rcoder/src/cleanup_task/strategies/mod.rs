//! 清理策略模块
//!
//! 定义不同服务类型的清理策略 trait

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::sync::Arc;

/// 容器销毁原因
///
/// 记录容器被销毁的具体原因，便于日志追踪和问题排查
#[derive(Debug, Clone, Serialize)]
pub enum DestroyReason {
    /// 闲置超时 - 所有项目都超过闲置时间限制
    /// - RCoder: 项目闲置超时
    /// - ComputerAgentRunner: 容器下所有项目都闲置
    IdleTimeout {
        /// 闲置时长（秒）
        idle_duration_secs: i64,
        /// 超时阈值（秒）
        timeout_secs: u64,
    },

    /// 孤立容器 - DuckDB 中没有对应记录
    /// - 容器存在但状态管理系统中没有记录
    /// - 可能是由于系统重启、异常退出等原因导致
    Orphaned {
        /// 容器创建时间
        created_at: DateTime<Utc>,
        /// 是否在保护期内
        was_protected: bool,
    },

    /// 手动停止 - 用户主动停止或重启
    /// - 通过 API 调用停止
    /// - 通过 Agent 生命周期管理停止
    ManualStop {
        /// 触发来源
        source: String,
    },
}

impl DestroyReason {
    /// 获取销毁原因的简短描述（用于日志）
    pub fn as_str(&self) -> &str {
        match self {
            DestroyReason::IdleTimeout { .. } => "闲置超时",
            DestroyReason::Orphaned { .. } => "孤立容器",
            DestroyReason::ManualStop { .. } => "手动停止",
        }
    }

    /// 获取销毁原因的详细描述
    pub fn description(&self) -> String {
        match self {
            DestroyReason::IdleTimeout {
                idle_duration_secs,
                timeout_secs,
            } => {
                format!(
                    "闲置超时 (闲置{}秒 / 超时{}秒)",
                    idle_duration_secs, timeout_secs
                )
            }
            DestroyReason::Orphaned {
                created_at,
                was_protected,
            } => {
                format!(
                    "孤立容器 (创建于{}, 保护期:{})",
                    created_at.format("%Y-%m-%d %H:%M:%S"),
                    was_protected
                )
            }
            DestroyReason::ManualStop { source } => {
                format!("manual stop (source:{})", source)
            }
        }
    }
}

/// 清理策略 trait
///
/// 不同服务类型有不同的容器管理策略：
/// - RCoder: 1容器 = 1项目，直接销毁
/// - ComputerAgentRunner: 1容器 = N项目，需要引用计数检查
#[async_trait]
pub trait CleanupStrategy: Send + Sync {
    /// 检查容器是否应该被销毁
    ///
    /// 返回 `Some(DestroyReason)` 表示应该销毁容器，并附带原因
    /// 返回 `None` 表示不应该销毁容器
    async fn should_destroy_container(
        &self,
        project_id: &str,
        context: &CleanupContext,
    ) -> Result<Option<DestroyReason>>;

    /// 获取容器的唯一标识符（用于查找容器）
    ///
    /// RCoder 返回 project_id
    /// ComputerAgentRunner 返回 user_id
    fn get_container_identifier(&self, project_info: &ProjectInfo) -> Result<String>;
}

/// 清理上下文
pub struct CleanupContext {
    pub state: Arc<crate::router::AppState>,
    pub config: super::config::CleanupConfig,
}

/// 项目信息摘要
pub struct ProjectInfo {
    pub project_id: String,
    pub user_id: Option<String>,
    pub last_activity: DateTime<Utc>,
}

pub mod computer_runner;
pub mod rcoder;
