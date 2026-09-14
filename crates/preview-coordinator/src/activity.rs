//! keep-alive activity 内存累积器（稳态零 PG 写；30s 批量刷盘 GREATEST 合并）。
//!
//! activity 不参与任何正确性判定（存活判定用宿主心跳），丢批仅推迟空闲回收
//! 检测，故允许缓冲。刷盘失败保留下次重试。
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use chrono::{DateTime, Utc};
use shared_types::ActivityFlushEntry;

#[derive(Default)]
pub struct ActivityAccumulator {
    /// preview_key → (instance_id, 最新 activity 时间, 落库最旧未确认时间)
    inner: Mutex<HashMap<String, Pending>>,
}

struct Pending {
    instance_id: String,
    /// 本批待写时间戳（GREATEST 语义下取最新即可）。
    latest: DateTime<Utc>,
    /// 首次进入脏集的时间（诊断刷盘延迟）。
    dirty_since: Instant,
}

impl ActivityAccumulator {
    pub fn record(&self, preview_key: &str, instance_id: &str, at: DateTime<Utc>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner
            .entry(preview_key.to_string())
            .and_modify(|p| {
                if p.instance_id == instance_id && at > p.latest {
                    p.latest = at;
                } else if p.instance_id != instance_id {
                    // 实例换代：覆盖为新实例条目
                    p.instance_id = instance_id.to_string();
                    p.latest = at;
                    p.dirty_since = Instant::now();
                }
            })
            .or_insert(Pending {
                instance_id: instance_id.to_string(),
                latest: at,
                dirty_since: Instant::now(),
            });
    }

    /// 取出脏条目（flush 成功后丢弃；失败由调用方 re-put 或下个周期重建——
    /// activity 会随下一次 keep-alive 再进来，失败重试不是正确性要求）。
    pub fn drain(&self) -> Vec<ActivityFlushEntry> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        inner
            .drain()
            .map(|(preview_key, p)| ActivityFlushEntry {
                preview_key,
                instance_id: p.instance_id,
                at: p.latest,
            })
            .collect()
    }

    pub fn dirty_count(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_latest_per_instance_and_replaces_on_new_instance() {
        let acc = ActivityAccumulator::default();
        let t1 = Utc::now();
        let t2 = t1 + chrono::Duration::seconds(5);
        acc.record("k", "i1", t1);
        acc.record("k", "i1", t2);
        // 旧时间戳不回退
        acc.record("k", "i1", t1);
        let drained = acc.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].instance_id, "i1");
        assert_eq!(drained[0].at, t2);
        assert_eq!(acc.dirty_count(), 0);
        // 新实例覆盖
        acc.record("k", "i2", t1);
        assert_eq!(acc.drain()[0].instance_id, "i2");
    }
}
