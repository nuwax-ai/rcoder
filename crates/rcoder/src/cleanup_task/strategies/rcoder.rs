//! RCoder 清理策略
//!
//! RCoder 模式: 1容器 = 1项目，直接销毁

use super::{CleanupContext, CleanupStrategy, ProjectInfo};
use anyhow::Result;
use async_trait::async_trait;

/// RCoder 清理策略
///
/// 在 RCoder 模式中，每个项目对应一个独立的容器
/// 清理时可以直接销毁容器，无需检查其他项目
pub struct RCoderStrategy;

#[async_trait]
impl CleanupStrategy for RCoderStrategy {
    async fn should_destroy_container(
        &self,
        _project_id: &str,
        _context: &CleanupContext,
    ) -> Result<bool> {
        // RCoder: 1容器=1项目，始终销毁容器
        Ok(true)
    }

    fn get_container_identifier(&self, project_info: &ProjectInfo) -> Result<String> {
        // RCoder: 容器标识符就是 project_id
        Ok(project_info.project_id.clone())
    }

    fn name(&self) -> &str {
        "RCoder"
    }
}
