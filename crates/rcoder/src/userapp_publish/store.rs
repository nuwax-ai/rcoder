//! 全局 publish/build 任务表(内存;短期)。终态任务按 TTL 保留,达容量上限优先淘汰最旧终态。
//!
//! 发布产物(持久)由 app_manager release index 持有,本表只管运行期任务句柄与进度流。
//! 结构参考 file-server `BuildTaskStore`(`crates/file-server/src/service/userapp/tasks.rs`)。
//!
//! M5 双模式：注入 [`PublishTaskPersistence`]（PG）后任务行入库——create 撞
//! `UNIQUE(app_id) WHERE terminal_at IS NULL` 映射 AppBusy 409（跨进程/跨副本安全）、
//! 终态/阶段经 task 钩子异步落库、get 未命中回查 PG 快照（跨重启状态可查）。
//! 内存对象（broadcast/ring/SSE 流）始终进程内；stream 仅支持本地活任务。

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Mutex;

use super::task::{OnStageFn, OnTerminalFn, PublishTask, TerminalPersist};
use super::types::{
    PublishTaskKind, PublishTaskSnapshot, PublishTaskStatus, PublishTaskStoreError,
};
use rcoder_storage::publish_repo::{PublishRepoError, PublishTaskPersistence, PublishTaskRecord};

/// 终态任务在内存中保留 24h,便于前端重连查询（也是 PG 行的 TTL）。
pub(crate) const TERMINAL_TASK_TTL_SECS: i64 = 24 * 60 * 60;
/// 防止异常调用方无限创建任务。达上限时优先淘汰最旧终态任务。
const MAX_RETAINED_TASKS: usize = 1_000;

/// 全局任务表(内存;短期。发布产物由 app_manager release index 持久)。
pub struct PublishTaskStore {
    map: Mutex<HashMap<String, Arc<PublishTask>>>,
    max_retained_tasks: usize,
    /// PG 持久化（None=纯内存模式，行为与历史版本一致）
    repo: Option<Arc<dyn PublishTaskPersistence>>,
}

impl PublishTaskStore {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max_retained_tasks: MAX_RETAINED_TASKS,
            repo: None,
        }
    }

    /// PG 模式构造（rcoder main 在 backend=postgres 时注入）
    pub fn with_repo(repo: Arc<dyn PublishTaskPersistence>) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max_retained_tasks: MAX_RETAINED_TASKS,
            repo: Some(repo),
        }
    }

    /// 持久化仓储（background_tasks 的 TTL 清理任务用；内存模式 None）
    pub fn repo(&self) -> Option<Arc<dyn PublishTaskPersistence>> {
        self.repo.clone()
    }

    #[cfg(test)]
    fn with_max_retained_tasks(max_retained_tasks: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max_retained_tasks,
            repo: None,
        }
    }

    pub async fn create(
        &self,
        app_id: String,
        project_id: String,
        kind: PublishTaskKind,
    ) -> Result<Arc<PublishTask>, PublishTaskStoreError> {
        let now = Utc::now().timestamp();
        let mut map = self.map.lock().await;
        map.retain(|_, existing| {
            let terminal_at = existing.terminal_at();
            terminal_at == 0 || now.saturating_sub(terminal_at) < TERMINAL_TASK_TTL_SECS
        });
        // U2 并发早拒绝:同 app 已有活跃任务(未终态)则 409,避免白跑 build 直到
        // activate 撞 pending 守卫才失败。检查与插入在同一把 map 锁内,并发 create 原子串行。
        // app_id 为任务锁外不可变字段、terminal_at 为原子读,扫描无需取任务 state 锁;
        // n≤MAX_RETAINED_TASKS,线性扫描即可,不引入二级索引。
        if let Some(busy) = map
            .values()
            .find(|t| t.app_id() == app_id && t.terminal_at() == 0)
        {
            return Err(PublishTaskStoreError::AppBusy {
                app_id,
                task_id: busy.id.clone(),
            });
        }
        while map.len() >= self.max_retained_tasks {
            let Some(oldest_terminal_id) = map
                .values()
                .filter(|existing| existing.terminal_at() > 0)
                .min_by_key(|existing| existing.created_at())
                .map(|existing| existing.id.clone())
            else {
                return Err(PublishTaskStoreError::CapacityExceeded {
                    limit: self.max_retained_tasks,
                });
            };
            map.remove(&oldest_terminal_id);
        }

        // PG 模式：显式生成 id → 先落库（唯一索引兜底跨进程并发）→ 成功才建带钩子的内存任务。
        // 注：此处持 map 锁跨 repo.create 的 PG 往返——有意为之。create 是人级频率
        // （build/publish 按钮触发），U2"同 app 单活跃任务"语义本就需要创建串行化；
        // 换乐观方案（放锁→PG→复锁）会引入容量/占用两个竞窗，收益为零。
        if let Some(repo) = &self.repo {
            let task_id = uuid::Uuid::now_v7().simple().to_string();
            let created_at = Utc::now();
            let record = PublishTaskRecord {
                task_id: task_id.clone(),
                app_id: app_id.clone(),
                project_id: project_id.clone(),
                kind: kind.as_pg_str().to_string(),
                state: PublishTaskStatus::Pending.as_pg_str().to_string(),
                stage: None,
                release_id: None,
                error: None,
                progress: None,
                owner_pod: Some(owner_pod_name()),
                created_at,
                terminal_at: None,
            };
            if let Err(repo_err) = repo.create(&record).await {
                return Err(map_repo_error(repo_err, app_id));
            }
            // 终态/阶段钩子：fire-and-forget spawn 异步落库（不阻塞 emit 热路径）
            let terminal_repo = Arc::clone(repo);
            let terminal_task_id = task_id.clone();
            let on_terminal: OnTerminalFn = Arc::new(move |payload: TerminalPersist| {
                let repo = Arc::clone(&terminal_repo);
                let task_id = terminal_task_id.clone();
                tokio::spawn(async move {
                    let at = chrono::DateTime::from_timestamp(payload.terminal_at, 0)
                        .unwrap_or_else(Utc::now);
                    if let Err(e) = repo
                        .update_terminal(
                            &task_id,
                            payload.status.as_pg_str(),
                            at,
                            payload.error.as_deref(),
                            payload.release_id.as_deref(),
                        )
                        .await
                    {
                        tracing::error!("[USERAPP_PUBLISH] terminal persist failed: {e}");
                    }
                });
            });
            let stage_repo = Arc::clone(repo);
            let stage_task_id = task_id.clone();
            let on_stage: OnStageFn = Arc::new(move |stage: &str| {
                let repo = Arc::clone(&stage_repo);
                let task_id = stage_task_id.clone();
                let stage = stage.to_string();
                tokio::spawn(async move {
                    if let Err(e) = repo.update_stage(&task_id, &stage, None).await {
                        tracing::error!("[USERAPP_PUBLISH] stage persist failed: {e}");
                    }
                });
            });
            let task = PublishTask::with_hooks(
                task_id,
                app_id,
                project_id,
                kind,
                Some(on_terminal),
                Some(on_stage),
            );
            map.insert(task.id.clone(), task.clone());
            return Ok(task);
        }

        let task = PublishTask::new(app_id, project_id, kind);
        map.insert(task.id.clone(), task.clone());
        Ok(task)
    }

    pub async fn get(&self, id: &str) -> Option<Arc<PublishTask>> {
        self.map.lock().await.get(id).cloned()
    }

    /// 快照查询：内存命中（活任务）→ 未命中回查 PG 行（跨重启/跨副本状态可查；
    /// `seq` 恒 0（无事件流），`updated_at` 取终态/创建时间戳）。
    pub async fn lookup_snapshot(&self, id: &str) -> Option<PublishTaskSnapshot> {
        if let Some(task) = self.get(id).await {
            return Some(task.snapshot().await);
        }
        let repo = self.repo.as_ref()?;
        let record = repo.get(id).await.ok()??;
        let created_at = record.created_at.timestamp();
        Some(PublishTaskSnapshot {
            id: record.task_id,
            app_id: record.app_id,
            project_id: record.project_id,
            kind: match record.kind.as_str() {
                "build" => PublishTaskKind::Build,
                _ => PublishTaskKind::Publish,
            },
            status: PublishTaskStatus::from_pg_str(&record.state),
            stage: record.stage,
            release_id: record.release_id,
            error: record.error,
            seq: 0,
            created_at,
            updated_at: record
                .terminal_at
                .map(|t| t.timestamp())
                .unwrap_or(created_at),
        })
    }
}

/// PG 仓储错误 → store 错误（Busy 携带 PG 侧冲突详情；Backend 如实 500）
fn map_repo_error(error: PublishRepoError, app_id: String) -> PublishTaskStoreError {
    match error {
        PublishRepoError::Busy(detail) => PublishTaskStoreError::AppBusy {
            task_id: format!("<pg-active:{detail}>"),
            app_id,
        },
        PublishRepoError::Backend(message) => PublishTaskStoreError::Backend(message),
    }
}

/// 本 Pod 名（owner_pod 诊断字段；K8s Downward API 注入 POD_NAME，退 HOSTNAME）
fn owner_pod_name() -> String {
    std::env::var("POD_NAME").unwrap_or_else(|_| std::env::var("HOSTNAME").unwrap_or_default())
}

impl Default for PublishTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::userapp_publish::types::PublishEvent;

    #[tokio::test]
    async fn store_rejects_new_task_when_all_capacity_is_active() {
        let store = PublishTaskStore::with_max_retained_tasks(2);
        for app_id in ["app-a", "app-b"] {
            store
                .create(app_id.into(), app_id.into(), PublishTaskKind::Build)
                .await
                .expect("active task within capacity");
        }

        let result = store
            .create("app-c".into(), "app-c".into(), PublishTaskKind::Build)
            .await;
        let error = match result {
            Ok(_) => panic!("active tasks must never be silently evicted"),
            Err(error) => error,
        };
        assert_eq!(error, PublishTaskStoreError::CapacityExceeded { limit: 2 });
        assert_eq!(store.map.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn store_evicts_terminal_task_before_rejecting_new_task() {
        let store = PublishTaskStore::with_max_retained_tasks(1);
        let completed = store
            .create("app-a".into(), "app-a".into(), PublishTaskKind::Build)
            .await
            .expect("first task");
        completed
            .emit(PublishEvent::Completed {
                release_id: "release-a".into(),
            })
            .await;

        let replacement = store
            .create("app-b".into(), "app-b".into(), PublishTaskKind::Build)
            .await
            .expect("terminal task should be evicted");
        let map = store.map.lock().await;
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&replacement.id));
        assert!(!map.contains_key(&completed.id));
    }

    /// U2:同 app 第二个活跃任务被拒(AppBusy 携带既有活跃任务 id)。
    #[tokio::test]
    async fn store_rejects_second_active_task_for_same_app() {
        let store = PublishTaskStore::new();
        let first = store
            .create("app-a".into(), "app-a".into(), PublishTaskKind::Publish)
            .await
            .expect("first task");

        let result = store
            .create("app-a".into(), "app-a".into(), PublishTaskKind::Publish)
            .await;
        assert_eq!(
            result.err(),
            Some(PublishTaskStoreError::AppBusy {
                app_id: "app-a".into(),
                task_id: first.id.clone(),
            }),
            "second active task for the same app must be rejected with AppBusy"
        );
        assert_eq!(store.map.lock().await.len(), 1);
    }

    /// U2:前一任务进入终态后,同 app 允许再建新任务。
    #[tokio::test]
    async fn store_allows_new_task_after_previous_task_reaches_terminal() {
        let store = PublishTaskStore::new();
        let first = store
            .create("app-a".into(), "app-a".into(), PublishTaskKind::Publish)
            .await
            .expect("first task");
        first
            .emit(PublishEvent::Failed {
                error: "build failed".into(),
            })
            .await;

        let second = store
            .create("app-a".into(), "app-a".into(), PublishTaskKind::Publish)
            .await
            .expect("terminal previous task must not block new task");
        assert_ne!(second.id, first.id);
    }

    /// U2:跨 app 不受 per-app 拒绝影响。
    #[tokio::test]
    async fn store_allows_concurrent_active_tasks_for_different_apps() {
        let store = PublishTaskStore::new();
        store
            .create("app-a".into(), "app-a".into(), PublishTaskKind::Publish)
            .await
            .expect("task for app-a");
        let other = store
            .create("app-b".into(), "app-b".into(), PublishTaskKind::Build)
            .await
            .expect("task for a different app must not be rejected");
        assert_eq!(other.app_id(), "app-b");
        assert_eq!(store.map.lock().await.len(), 2);
    }
}
