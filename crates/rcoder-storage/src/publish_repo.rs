//! PublishTask 持久化契约（rcoder userapp_publish ↔ PG 实现的数据边界）
//!
//! PublishTaskStore 的内存对象（broadcast/ring/seq 进度流）保持进程内；
//! 本契约持久化任务行（状态跨重启/跨副本可查）。入出参全部原语字段，
//! rcoder 侧负责枚举↔字符串映射——sqlx 保持只在 rcoder-storage 一个 crate。

use chrono::{DateTime, Utc};
use serde_json::Value;

/// publish_tasks 表行（原语字段，无 rcoder 侧类型依赖）
#[derive(Debug, Clone)]
pub struct PublishTaskRecord {
    /// 任务 ID（uuid v7，应用侧生成）
    pub task_id: String,
    pub app_id: String,
    pub project_id: String,
    /// build / publish
    pub kind: String,
    /// pending / running / cancelling / completed / failed / cancelled
    pub state: String,
    /// 当前阶段标识
    pub stage: Option<String>,
    /// 产出 release ID（completed 回填）
    pub release_id: Option<String>,
    /// 失败原因
    pub error: Option<String>,
    /// 进度摘要 JSON
    pub progress: Option<Value>,
    /// 执行该任务的 rcoder Pod（诊断用）
    pub owner_pod: Option<String>,
    pub created_at: DateTime<Utc>,
    /// 终态时间戳（None=未终态）
    pub terminal_at: Option<DateTime<Utc>>,
}

/// 仓储错误（AppBusy=同 app 活跃任务冲突 409；Backend=其他）
#[derive(Debug, thiserror::Error)]
pub enum PublishRepoError {
    /// 同 app 已有活跃任务（唯一索引冲突，映射现有 AppBusy 409 语义）
    #[error("app already has an active task: {0}")]
    Busy(String),
    #[error("backend: {0}")]
    Backend(String),
}

/// PublishTask 持久化契约（PG 实现；内存模式 None）
#[async_trait::async_trait]
pub trait PublishTaskPersistence: Send + Sync {
    /// 创建任务行；同 app 已有活跃任务返回 [`PublishRepoError::Busy`]
    /// （`UNIQUE(app_id) WHERE terminal_at IS NULL` 部分唯一索引，跨副本安全）。
    async fn create(&self, record: &PublishTaskRecord) -> Result<(), PublishRepoError>;

    /// 查询单行（get 的 PG 回退路径）
    async fn get(&self, task_id: &str) -> Result<Option<PublishTaskRecord>, PublishRepoError>;

    /// 终态落库（state/terminal_at/error/release_id）
    async fn update_terminal(
        &self,
        task_id: &str,
        state: &str,
        terminal_at: DateTime<Utc>,
        error: Option<&str>,
        release_id: Option<&str>,
    ) -> Result<(), PublishRepoError>;

    /// 阶段/进度摘要更新（节流调用）
    async fn update_stage(
        &self,
        task_id: &str,
        stage: &str,
        progress: Option<&Value>,
    ) -> Result<(), PublishRepoError>;

    /// 启动恢复：未终态任务全部标记 failed（orchestrator 随进程消亡，running 必为僵尸）。
    /// 返回恢复行数。
    async fn recover_running(&self, reason: &str) -> Result<u64, PublishRepoError>;

    /// 清理过期终态行（TTL 秒）。返回删除行数。
    async fn purge_expired(&self, ttl_secs: i64) -> Result<u64, PublishRepoError>;
}
