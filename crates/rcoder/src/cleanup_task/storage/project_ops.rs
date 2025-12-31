//! 项目操作辅助函数

use std::time::Duration;

/// 项目操作辅助函数
pub struct ProjectOps;

impl ProjectOps {
    /// 判断项目是否活跃
    pub fn is_project_active(
        project: &duckdb_manager::ProjectRecord,
        active_window: Duration,
    ) -> bool {
        let now = chrono::Utc::now();
        let idle_duration = now - project.last_activity;
        idle_duration.num_seconds() < active_window.as_secs() as i64
    }
}
