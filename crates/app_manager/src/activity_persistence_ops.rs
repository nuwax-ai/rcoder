//! 影子持久化操作（从 activity_registry.rs 拆出，extension-impl）
//!
//! PG 模式下的数据面：持久化注入、启动恢复加载（apply_loaded）、
//! 脏行/删除收集（flusher 周期消费）。wake/回收协调机制仍在本体文件。

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
            if let Some(at) = row.last_accessed {
                self.last_accessed.insert(row.app_id.clone(), at);
            }
            if row.stopped {
                self.stopped.insert(row.app_id.clone());
            }
            if row.wake_blocked {
                self.wake_blocked.insert(row.app_id.clone());
            }
        }
    }

    /// 收集脏行快照并清脏（flusher 周期调用；行值取 collect 时刻的当前状态）。
    pub fn collect_dirty(&self) -> Vec<ActivityRow> {
        let app_ids: Vec<String> = self.dirty.iter().map(|k| k.key().clone()).collect();
        let mut rows = Vec::with_capacity(app_ids.len());
        for app_id in app_ids {
            self.dirty.remove(&app_id);
            // app 可能已被 forget：若内存无任何痕迹则跳过（删除走 drain_deleted）
            let last = self.last_accessed.get(&app_id).map(|r| *r);
            let stopped = self.stopped.contains(&app_id);
            let wake_blocked = self.wake_blocked.contains(&app_id);
            if last.is_none() && !stopped && !wake_blocked {
                continue;
            }
            rows.push(ActivityRow {
                app_id,
                last_accessed: last,
                stopped,
                wake_blocked,
            });
        }
        rows
    }

    /// 取出待删除的 app_id 列表（flusher 在 upsert 前先执行删除）
    pub fn drain_deleted(&self) -> Vec<String> {
        let ids: Vec<String> = self.deleted.iter().map(|k| k.key().clone()).collect();
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
    /// flush 失败后重标脏（下轮 flusher 重试）。
    ///
    /// collect_dirty 取走标记到 flush 落库之间存在失败窗口：不重标则该批
    /// 数据在下次变更前不会再持久化。期间若有新 touch 已自行标脏，
    /// 幂等 insert 无害。
    pub fn re_dirty(&self, app_ids: &[String]) {
        for id in app_ids {
            self.dirty.insert(id.clone());
        }
    }

    /// delete 失败后重登删除队列（forget_app 的行不被遗漏）
    pub fn re_delete(&self, app_ids: &[String]) {
        for id in app_ids {
            self.deleted.insert(id.clone());
        }
    }
}
