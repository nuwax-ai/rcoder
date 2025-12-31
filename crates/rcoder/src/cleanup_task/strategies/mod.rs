//! 清理策略模块
//!
//! 定义不同服务类型的清理策略 trait

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::Arc;

/// 清理策略 trait
///
/// 不同服务类型有不同的容器管理策略：
/// - RCoder: 1容器 = 1项目，直接销毁
/// - ComputerAgentRunner: 1容器 = N项目，需要引用计数检查
#[async_trait]
pub trait CleanupStrategy: Send + Sync {
    /// 检查容器是否应该被销毁
    ///
    /// 返回 true 表示应该销毁容器，false 表示只清理项目记录
    async fn should_destroy_container(
        &self,
        project_id: &str,
        context: &CleanupContext,
    ) -> Result<bool>;

    /// 获取容器的唯一标识符（用于查找容器）
    ///
    /// RCoder 返回 project_id
    /// ComputerAgentRunner 返回 user_id
    fn get_container_identifier(&self, project_info: &ProjectInfo) -> Result<String>;

    /// 策略名称
    fn name(&self) -> &str;
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
    pub service_type: shared_types::ServiceType,
    pub container_id: Option<String>,
    pub last_activity: DateTime<Utc>,
}

pub mod computer_runner;
pub mod rcoder;
