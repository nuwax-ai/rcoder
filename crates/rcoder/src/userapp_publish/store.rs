//! 全局 publish/build 任务表(内存;短期)。终态任务按 TTL 保留,达容量上限优先淘汰最旧终态。
//!
//! 发布产物(持久)由 app_manager release index 持有,本表只管运行期任务句柄与进度流。
//! 结构参考 file-server `BuildTaskStore`(`crates/file-server/src/service/userapp/tasks.rs`)。

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Mutex;

use super::task::PublishTask;
use super::types::{PublishTaskKind, PublishTaskStoreError};

/// 终态任务在内存中保留 24h,便于前端重连查询。
const TERMINAL_TASK_TTL_SECS: i64 = 24 * 60 * 60;
/// 防止异常调用方无限创建任务。达上限时优先淘汰最旧终态任务。
const MAX_RETAINED_TASKS: usize = 1_000;

/// 全局任务表(内存;短期。发布产物由 app_manager release index 持久)。
pub struct PublishTaskStore {
    map: Mutex<HashMap<String, Arc<PublishTask>>>,
    max_retained_tasks: usize,
}

impl PublishTaskStore {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max_retained_tasks: MAX_RETAINED_TASKS,
        }
    }

    #[cfg(test)]
    fn with_max_retained_tasks(max_retained_tasks: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max_retained_tasks,
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
        let task = PublishTask::new(app_id, project_id, kind);
        map.insert(task.id.clone(), task.clone());
        Ok(task)
    }

    pub async fn get(&self, id: &str) -> Option<Arc<PublishTask>> {
        self.map.lock().await.get(id).cloned()
    }
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
