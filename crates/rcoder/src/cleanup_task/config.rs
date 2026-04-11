//! 清理任务配置和统计

use chrono::{DateTime, Utc};
use std::time::Duration;

/// 清理配置
#[derive(Debug, Clone)]
pub struct CleanupConfig {
    /// 闲置超时时间（默认30分钟）
    pub idle_timeout: Duration,
    /// 清理检查间隔（默认5分钟）
    pub cleanup_interval: Duration,
    /// Docker容器停止超时时间（默认30秒）
    #[allow(dead_code)] // 保留用于未来的超时配置
    pub docker_stop_timeout: Duration,
    /// 容器最小保护时间（默认5分钟）
    pub container_protection_duration: Duration,
    /// Agent 活跃判断时间窗口（默认5分钟）
    pub active_window: Duration,
    /// 日志目录路径
    pub log_dir: String,
    /// 日志保留时长
    pub log_retention_duration: Duration,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(30 * 60),
            cleanup_interval: Duration::from_secs(5 * 60),
            docker_stop_timeout: Duration::from_secs(30),
            container_protection_duration: Duration::from_secs(5 * 60),
            active_window: Duration::from_secs(5 * 60),
            log_dir: "/app/logs/container".to_string(),
            log_retention_duration: Duration::from_secs(7 * 24 * 60 * 60),
        }
    }
}

/// 清理任务统计信息
#[derive(Debug, Clone, Default)]
pub struct CleanupStats {
    /// 总共清理的agent数量
    pub total_cleaned: u64,
    /// 成功清理的agent数量
    pub success_cleaned: u64,
    /// 清理失败的agent数量
    pub failed_cleaned: u64,
    /// 销毁的容器数量
    pub containers_destroyed: u64,
    /// 最后清理时间
    pub last_cleanup: Option<DateTime<Utc>>,
}

impl CleanupStats {
    /// 获取清理成功率
    pub fn success_rate(&self) -> f64 {
        if self.total_cleaned == 0 {
            0.0
        } else {
            (self.success_cleaned as f64 / self.total_cleaned as f64) * 100.0
        }
    }

    /// 获取格式化的统计摘要
    pub fn summary(&self) -> String {
        format!(
            "Total cleanup: {}, success: {}, failed: {}, containers destroyed: {}, success rate: {:.1}%",
            self.total_cleaned,
            self.success_cleaned,
            self.failed_cleaned,
            self.containers_destroyed,
            self.success_rate()
        )
    }
}
