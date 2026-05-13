//! 日志清理模块
//!
//! 负责清理 /app/logs/container 目录下的过期日志文件

#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;
use std::vec::Vec;
use tokio::fs;
use tracing::{debug, info, warn};

/// 日志清理器
pub struct LogCleaner {
    /// 日志目录路径
    log_dir: String,
    /// 日志保留时长（默认 7 天）
    retention_duration: Duration,
}

impl LogCleaner {
    /// 创建新的日志清理器
    pub fn new(log_dir: impl Into<String>, retention_days: u64) -> Self {
        Self {
            log_dir: log_dir.into(),
            retention_duration: Duration::from_secs(retention_days * 24 * 60 * 60),
        }
    }

    /// 执行一次日志清理
    ///
    /// # 返回
    /// 返回清理的文件数量和释放的字节数
    pub async fn cleanup_once(&self) -> Result<LogCleanupStats, std::io::Error> {
        let log_path = Path::new(&self.log_dir);

        // 检查目录是否存在
        if !log_path.exists() {
            debug!(
                "📋 [log_cleaner] Log directory does not exist, skipping cleanup: {}",
                self.log_dir
            );
            return Ok(LogCleanupStats::default());
        }

        // 检查是否是目录
        if !log_path.is_dir() {
            warn!(
                "📋 [log_cleaner] Path is not a directory, skip cleanup: {}",
                self.log_dir
            );
            return Ok(LogCleanupStats::default());
        }

        info!(
            "🧹 [log_cleaner] Starting log directory cleanup: {}, retention: {} days",
            self.log_dir,
            self.retention_duration.as_secs() / 86400
        );

        let mut stats = LogCleanupStats::default();
        let cutoff_time = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => duration.as_secs(),
            Err(e) => {
                warn!("📋 [log_cleaner] Invalid timestamp, skip cleanup: {}", e);
                return Ok(LogCleanupStats::default());
            }
        };
        let cutoff_time = cutoff_time.saturating_sub(self.retention_duration.as_secs());

        // 读取目录内容
        let mut entries = match fs::read_dir(log_path).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("📋 [log_cleaner] Failed to read directory: {}", e);
                return Ok(LogCleanupStats::default());
            }
        };

        // 遍历目录中的文件和子目录
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let metadata = match entry.metadata().await {
                Ok(m) => m,
                Err(e) => {
                    debug!(
                        "📋 [log_cleaner] get file failed: {:?} - {}",
                        path, e
                    );
                    continue;
                }
            };

            // 获取修改时间（更可靠，所有文件系统都支持）
            let modified = match metadata.modified() {
                Ok(time) => time,
                Err(e) => {
                    debug!("📋 [log_cleaner] get modified time failed: {:?} - {}", path, e);
                    continue;
                }
            };

            // 转换为 Unix 时间戳
            let modified_secs = match modified.duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => duration.as_secs(),
                Err(_) => {
                    debug!("📋 [log_cleaner] skip: {:?}", path);
                    continue;
                }
            };

            // 判断是否过期（基于修改时间）
            if modified_secs < cutoff_time {
                if metadata.is_file() {
                    // 删除过期文件
                    let file_size = metadata.len();

                    match fs::remove_file(&path).await {
                        Ok(_) => {
                            stats.files_deleted += 1;
                            stats.bytes_freed += file_size;
                            debug!(
                                "🗑️ [log_cleaner] Deleting expired file: {:?} ({:.2} MB)",
                                path,
                                file_size as f64 / 1024.0 / 1024.0
                            );
                        }
                        Err(e) => {
                            stats.failed_deletions += 1;
                            warn!("📋 [log_cleaner] Failed to delete file: {:?} - {}", path, e);
                        }
                    }
                } else if metadata.is_dir() {
                    // 删除过期目录（递归删除整个目录）
                    match fs::remove_dir_all(&path).await {
                        Ok(_) => {
                            stats.dirs_deleted += 1;
                            debug!("🗑️ [log_cleaner] Deleted directory: {:?}", path);
                        }
                        Err(e) => {
                            stats.failed_deletions += 1;
                            warn!(
                                "📋 [log_cleaner] Failed to delete directory: {:?} - {}",
                                path, e
                            );
                        }
                    }
                }
            }
        }

        if stats.files_deleted > 0 || stats.dirs_deleted > 0 {
            info!(
                "✅ [log_cleaner] Log cleanup completed: deleted {} files, {} dirs, freed {:.2} MB",
                stats.files_deleted,
                stats.dirs_deleted,
                stats.bytes_freed as f64 / 1024.0 / 1024.0
            );
        } else {
            info!("[log_cleaner] Cleanup completed");
        }

        Ok(stats)
    }

    /// 获取日志目录路径
    pub fn log_dir(&self) -> &str {
        &self.log_dir
    }

    /// 获取保留时长（秒）
    pub fn retention_duration(&self) -> Duration {
        self.retention_duration
    }
}

/// 日志清理统计
#[derive(Debug, Clone, Default)]
pub struct LogCleanupStats {
    /// 删除的文件数量
    pub files_deleted: u64,
    /// 删除的目录数量
    pub dirs_deleted: u64,
    /// 释放的字节数
    pub bytes_freed: u64,
    /// 删除失败的文件/目录数量
    pub failed_deletions: u64,
}

impl LogCleanupStats {
    /// 获取格式化的统计摘要
    pub fn summary(&self) -> String {
        if self.files_deleted == 0 && self.dirs_deleted == 0 && self.failed_deletions == 0 {
            "No expired logs".to_string()
        } else {
            let mut parts = Vec::new();
            if self.files_deleted > 0 {
                parts.push(format!(
                    "Deleted: {} files, freed: {:.2} MB",
                    self.files_deleted,
                    self.bytes_freed as f64 / 1024.0 / 1024.0
                ));
            }
            if self.dirs_deleted > 0 {
                parts.push(format!("deleted: {} dirs", self.dirs_deleted));
            }
            if self.failed_deletions > 0 {
                parts.push(format!("failed: {}", self.failed_deletions));
            }
            parts.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_log_cleanup_basic() {
        // 创建临时测试目录
        let temp_dir = TempDir::new().unwrap();
        let log_dir = temp_dir.path().join("logs");

        // 创建日志目录
        fs::create_dir_all(&log_dir).await.unwrap();

        // 创建一些测试日志文件
        let test_files = vec![
            ("old_log_1.txt", "old content 1"),
            ("old_log_2.txt", "old content 2"),
            ("recent_log.txt", "recent content"),
        ];

        for (filename, content) in &test_files {
            let file_path = log_dir.join(filename);
            let mut file = File::create(&file_path).unwrap();
            file.write_all(content.as_bytes()).unwrap();
        }

        // 设置旧文件的修改时间为 15 天前
        let old_time =
            std::time::SystemTime::now() - std::time::Duration::from_secs(15 * 24 * 60 * 60);
        for (filename, _) in &test_files[..2] {
            let file_path = log_dir.join(filename);
            if let Err(e) = filetime::set_file_mtime(&file_path, old_time.into()) {
                // 系统不支持设置修改时间，跳过此测试
                println!(" set mtime not supported, skip: {}", e);
                return;
            }
        }

        // 创建日志清理器，保留 10 天
        let cleaner = LogCleaner::new(log_dir.to_str().unwrap(), 10);

        // 执行清理
        let stats = cleaner.cleanup_once().await.unwrap();

        // 验证结果
        assert_eq!(stats.files_deleted, 2);
        assert!(log_dir.join("recent_log.txt").exists());
        assert!(!log_dir.join("old_log_1.txt").exists());
        assert!(!log_dir.join("old_log_2.txt").exists());
    }

    #[tokio::test]
    async fn test_cleanup_nonexistent_dir() {
        let cleaner = LogCleaner::new("/nonexistent/path", 10);
        let stats = cleaner.cleanup_once().await.unwrap();
        assert_eq!(stats.files_deleted, 0);
        assert_eq!(stats.failed_deletions, 0);
    }
}
