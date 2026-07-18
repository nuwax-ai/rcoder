//! 开发日志内存缓存，对齐 nuwax `logCacheManager`，并用文件快照避免返回写入后的旧内容。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::Config;
use crate::error::{AppError, AppResult};
use crate::service::dev_server::log::{LogFileSnapshot, LogLine, ReadDevLogResult};

#[derive(Clone)]
struct Entry {
    snapshot: LogFileSnapshot,
    lines: Vec<LogLine>,
    last_access: Instant,
    size_bytes: u64,
}

pub struct LogCacheManager {
    enabled: bool,
    duration: Duration,
    max_entries: usize,
    max_file_size: u64,
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub enabled: bool,
    pub cache_size: u64,
    pub max_cache_entries: u64,
    pub cache_duration: u64,
    pub max_file_size_bytes: u64,
    pub total_cache_size_bytes: u64,
}

impl LogCacheManager {
    pub fn new(config: &Config) -> Self {
        Self {
            enabled: config.log_cache_enabled,
            duration: Duration::from_millis(config.log_cache_duration_ms),
            max_entries: config.log_cache_max_entries,
            max_file_size: config.log_cache_max_file_size_bytes,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(
        &self,
        project_id: &str,
        snapshot: &LogFileSnapshot,
        start_index: usize,
    ) -> AppResult<Option<ReadDevLogResult>> {
        if !self.enabled {
            return Ok(None);
        }
        let now = Instant::now();
        let mut entries = self.lock()?;
        entries.retain(|_, entry| now.duration_since(entry.last_access) <= self.duration);
        let Some(entry) = entries.get_mut(project_id) else {
            return Ok(None);
        };
        if entry.snapshot != *snapshot {
            entries.remove(project_id);
            return Ok(None);
        }
        entry.last_access = now;
        let total = entry.lines.len();
        let start = start_index.saturating_sub(1).min(total);
        let logs = entry.lines[start..]
            .iter()
            .enumerate()
            .map(|(offset, line)| LogLine {
                line: start + offset + 1,
                content: line.content.clone(),
            })
            .collect();
        Ok(Some(ReadDevLogResult {
            logs,
            total_lines: total,
            start_index: start + 1,
            log_file_name: snapshot.file_name.clone(),
        }))
    }

    pub fn insert(
        &self,
        project_id: &str,
        snapshot: LogFileSnapshot,
        result: &ReadDevLogResult,
    ) -> AppResult<bool> {
        if !self.enabled || snapshot.size_bytes > self.max_file_size || self.max_entries == 0 {
            return Ok(false);
        }
        let mut entries = self.lock()?;
        if !entries.contains_key(project_id) && entries.len() >= self.max_entries {
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                entries.remove(&key);
            }
        }
        entries.insert(
            project_id.to_string(),
            Entry {
                size_bytes: snapshot.size_bytes,
                snapshot,
                lines: result.logs.clone(),
                last_access: Instant::now(),
            },
        );
        Ok(true)
    }

    pub fn delete(&self, project_id: &str) -> AppResult<()> {
        self.lock()?.remove(project_id);
        Ok(())
    }

    pub fn clear(&self) -> AppResult<()> {
        self.lock()?.clear();
        Ok(())
    }

    pub fn stats(&self) -> AppResult<CacheStats> {
        let now = Instant::now();
        let mut entries = self.lock()?;
        entries.retain(|_, entry| now.duration_since(entry.last_access) <= self.duration);
        let total_cache_size_bytes = entries.values().map(|entry| entry.size_bytes).sum();
        let max_file_size_bytes = entries
            .values()
            .map(|entry| entry.size_bytes)
            .max()
            .unwrap_or(0);
        Ok(CacheStats {
            enabled: self.enabled,
            cache_size: entries.len() as u64,
            max_cache_entries: self.max_entries as u64,
            cache_duration: self.duration.as_millis() as u64,
            max_file_size_bytes,
            total_cache_size_bytes,
        })
    }

    fn lock(&self) -> AppResult<std::sync::MutexGuard<'_, HashMap<String, Entry>>> {
        self.entries
            .lock()
            .map_err(|error| AppError::system(format!("log cache lock poisoned: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn manager() -> LogCacheManager {
        LogCacheManager {
            enabled: true,
            duration: Duration::from_secs(60),
            max_entries: 2,
            max_file_size: 1024,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn snapshot(name: &str, size: u64) -> LogFileSnapshot {
        LogFileSnapshot {
            file_name: name.to_string(),
            size_bytes: size,
            modified: SystemTime::UNIX_EPOCH,
        }
    }

    fn result(name: &str) -> ReadDevLogResult {
        ReadDevLogResult {
            logs: vec![
                LogLine {
                    line: 1,
                    content: "one".to_string(),
                },
                LogLine {
                    line: 2,
                    content: "two".to_string(),
                },
            ],
            total_lines: 2,
            start_index: 1,
            log_file_name: name.to_string(),
        }
    }

    #[test]
    fn cache_hit_slices_from_requested_line() {
        let manager = manager();
        let snapshot = snapshot("dev.log", 8);
        assert!(
            manager
                .insert("project", snapshot.clone(), &result("dev.log"))
                .expect("insert cache")
        );
        let hit = manager
            .get("project", &snapshot, 2)
            .expect("get cache")
            .expect("cache hit");
        assert_eq!(hit.total_lines, 2);
        assert_eq!(hit.logs.len(), 1);
        assert_eq!(hit.logs[0].line, 2);
        assert_eq!(hit.logs[0].content, "two");
    }

    #[test]
    fn changed_snapshot_invalidates_entry() {
        let manager = manager();
        let old = snapshot("dev.log", 8);
        manager
            .insert("project", old, &result("dev.log"))
            .expect("insert cache");
        assert!(
            manager
                .get("project", &snapshot("dev.log", 9), 1)
                .expect("get cache")
                .is_none()
        );
        assert_eq!(manager.stats().expect("cache stats").cache_size, 0);
    }
}
