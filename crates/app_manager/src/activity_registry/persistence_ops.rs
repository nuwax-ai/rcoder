//! 影子持久化操作（activity_registry 目录子模块，extension-impl）
//!
//! PG 模式下的数据面：持久化注入、启动恢复加载（apply_loaded）、
//! 脏行/删除收集（flusher 周期消费）。wake/回收协调机制见同目录 wake.rs 与 mod.rs。

use std::sync::Arc;

use shared_types::ActivityRow;
use tracing::warn;

use super::AppActivityRegistry;

impl AppActivityRegistry {
    /// 注入影子持久化(PG 模式 main 在连接建立后调用)。返回注入前已注册的
    /// `persistence`(便于测试替换;生产忽略)。
    pub fn set_persistence(&self, p: Arc<dyn shared_types::ActivityPersistence>) {
        if self.persistence.set(p).is_err() {
            warn!("[ACTIVITY] set_persistence called twice; keeping existing");
        }
    }

    /// 已注入的影子持久化（flusher 用；内存模式 None）
    pub fn persistence(&self) -> Option<Arc<dyn shared_types::ActivityPersistence>> {
        self.persistence.get().cloned()
    }

    /// 启动恢复：PG 全量加载写入内存（不标脏，避免回写风暴）。
    /// 必须在 AppService::new（rebuild_stopped_apps）**之前**调用，
    /// 使 rebuild 仅对未加载到的 app `seed_accessed`（保住历史活跃时间）。
    pub fn apply_loaded(&self, rows: Vec<ActivityRow>) {
        for row in rows {
            let mut identities = self.identities.lock().unwrap_or_else(|p| p.into_inner());
            let existing = identities.get(&row.app_id);
            if existing.is_some_and(|(id, epoch)| {
                *epoch > row.lifecycle_epoch
                    || (*epoch == row.lifecycle_epoch && id != &row.lifecycle_id)
            }) {
                continue;
            }
            if existing.is_some_and(|(id, _)| id != &row.lifecycle_id) {
                self.last_accessed.remove(&row.app_id);
            }
            identities.insert(row.app_id.clone(), (row.lifecycle_id, row.lifecycle_epoch));
            if let Some(at) = row.last_accessed {
                self.merge_accessed(&row.app_id, at);
            }
        }
    }

    /// Capture generation and timestamp under the same registration lock.
    pub fn collect_dirty(&self) -> Vec<ActivityRow> {
        let identities = self.identities.lock().unwrap_or_else(|p| p.into_inner());
        let app_ids: Vec<String> = self.dirty.iter().map(|k| k.key().clone()).collect();
        let mut rows = Vec::new();
        for app_id in app_ids {
            let Some((lifecycle_id, lifecycle_epoch)) = identities.get(&app_id) else {
                continue;
            };
            self.dirty.remove(&app_id);
            let last_accessed = self.last_accessed.get(&app_id).map(|r| *r);
            if last_accessed.is_some() {
                rows.push(ActivityRow {
                    app_id,
                    lifecycle_id: lifecycle_id.clone(),
                    lifecycle_epoch: *lifecycle_epoch,
                    last_accessed,
                });
            }
        }
        rows
    }

    pub fn drain_deleted(&self) -> Vec<(String, String)> {
        let ids: Vec<_> = self.deleted.iter().map(|key| key.key().clone()).collect();
        for id in &ids {
            self.deleted.remove(id);
        }
        ids
    }

    /// 标脏（持久化注入后才有意义；无注入时为纯集合写，开销可忽略）
    pub(super) fn note_dirty(&self, app_id: &str) {
        self.dirty.insert(app_id.to_string());
    }
}

impl AppActivityRegistry {
    /// delete 失败后重登删除队列（forget_app 的行不被遗漏）
    pub fn re_delete(&self, app_ids: &[(String, String)]) {
        for id in app_ids {
            self.deleted.insert(id.clone());
        }
    }
}

impl AppActivityRegistry {
    /// Caller supplies the durable root it observed. Epoch ordering prevents an
    /// older lookup completing late from rebinding a recreated app backwards.
    pub fn bind_lifecycle(&self, app_id: &str, lifecycle_id: &str, epoch: i64) -> bool {
        if lifecycle_id.is_empty() || epoch < 1 {
            return false;
        }
        let mut identities = self.identities.lock().unwrap_or_else(|p| p.into_inner());
        self.bind_lifecycle_locked(&mut identities, app_id, lifecycle_id, epoch)
    }

    pub(super) fn bind_lifecycle_locked(
        &self,
        identities: &mut std::collections::HashMap<String, (String, i64)>,
        app_id: &str,
        lifecycle_id: &str,
        epoch: i64,
    ) -> bool {
        if lifecycle_id.is_empty() || epoch < 1 {
            return false;
        }
        if let Some((previous, old_epoch)) = identities.get(app_id) {
            if *old_epoch > epoch || (*old_epoch == epoch && previous != lifecycle_id) {
                return false;
            }
            if previous == lifecycle_id {
                return *old_epoch == epoch;
            }
            self.last_accessed.remove(app_id);
            self.dirty.remove(app_id);
        }
        self.invalidate_access_identity(app_id);
        identities.insert(app_id.to_owned(), (lifecycle_id.to_owned(), epoch));
        true
    }

    pub fn lifecycle_identity(&self, app_id: &str) -> Option<(String, i64)> {
        self.identities
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(app_id)
            .cloned()
    }
}

impl AppActivityRegistry {
    /// Flush captured lifecycle rows. Never lose a failed batch or report an
    /// incomplete final flush as a successful shutdown.
    pub async fn flush_pending(&self) -> anyhow::Result<()> {
        let Some(persistence) = self.persistence() else {
            return Ok(());
        };
        let deleted = self.drain_deleted();
        for (index, (app_id, lifecycle_id)) in deleted.iter().enumerate() {
            if let Err(error) = persistence.delete(app_id, lifecycle_id).await {
                self.re_delete(&deleted[index..]);
                return Err(error.context("Failed to flush activity deletions"));
            }
        }
        let rows = self.collect_dirty();
        if !rows.is_empty()
            && let Err(error) = persistence.flush_batch(rows.clone()).await
        {
            self.re_dirty_rows(&rows);
            return Err(error.context("Failed to flush activity timestamps"));
        }
        Ok(())
    }

    fn re_dirty_rows(&self, rows: &[ActivityRow]) {
        let identities = self.identities.lock().unwrap_or_else(|p| p.into_inner());
        for row in rows {
            if identities.get(&row.app_id) == Some(&(row.lifecycle_id.clone(), row.lifecycle_epoch))
            {
                self.dirty.insert(row.app_id.clone());
            }
        }
    }

    pub fn merge_lifecycle_accessed(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        epoch: i64,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        let identities = self.identities.lock().unwrap_or_else(|p| p.into_inner());
        if identities.get(app_id) != Some(&(lifecycle_id.to_owned(), epoch)) {
            return None;
        }
        Some(self.merge_accessed(app_id, at))
    }
}
