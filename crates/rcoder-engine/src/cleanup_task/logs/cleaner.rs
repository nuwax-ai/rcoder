//! 日志清理器：删除过期的日志文件和过期的日志目录。
//!
//! # 删除判据
//!
//! 目录的修改时间只用来判断"是否已超过保留期"，**不单独决定删除**。一个目录会被
//! `remove_dir_all` 删掉，必须同时满足：
//!
//! 1. 修改时间早于保留期；
//! 2. **当前没有任何容器把它当 bind mount 源**（运行中的、以及已停止未删除的都算）。
//!
//! 第 2 条是本次修复的核心：这些目录正是 RCoder 为每个容器创建的日志目录，同时也是
//! 该容器内 `/app/container-logs` 的 bind mount 源。容器还在时删掉它，容器里的这个路径
//! 就失效了（`ls` 为空、新建文件报 `ENOENT`），依赖它的 agent 会启动失败。
//!
//! 为什么按"是否被挂载"判断，而不是按"容器名叫什么还在不在"：容器改名后原名 inspect
//! 返回 404，但它的挂载还在——按名字判断会把运行中容器的日志目录删掉。挂载清单同时
//! 覆盖已停止未删除的容器，所以"停了但还没回收"的容器同样受保护。
//!
//! # 拿不到清单时
//!
//! 清单获取失败（Docker/K8s 无应答等）时，本轮**不删除任何目录**，只告警。
//! 没有任何路径会把"查不到"当成"没有挂载"。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tracing::{debug, info, warn};

use super::owner::MountedPathsProbe;

/// 一天的秒数：保留天数换算秒数的乘数
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// 日志清理器
pub struct LogCleaner {
    /// 日志目录路径
    log_dir: String,
    /// 日志保留时长
    retention_duration: Duration,
    /// 当前被容器挂载的宿主机路径清单
    mounted_paths: Arc<dyn MountedPathsProbe>,
}

impl LogCleaner {
    /// 创建新的日志清理器
    ///
    /// `retention_days` 来自配置文件（`cleanup_config.log_cleanup.log_retention_days`），
    /// 是不受信任的输入：换算成秒时必须用 `saturating_mul`。直接相乘在 debug 构建下会
    /// panic，在 release 构建下会静默回绕——例如 `144115188075855872` 天会回绕成 0 秒，
    /// 等于把保留期变成 0，任何目录都会被判为过期。
    pub fn new(
        log_dir: impl Into<String>,
        retention_days: u64,
        mounted_paths: Arc<dyn MountedPathsProbe>,
    ) -> Self {
        Self {
            log_dir: log_dir.into(),
            retention_duration: Duration::from_secs(retention_days.saturating_mul(SECS_PER_DAY)),
            mounted_paths,
        }
    }

    /// 执行一次日志清理
    ///
    /// # 返回
    /// 返回清理的文件数量、目录数量和释放的字节数
    pub async fn cleanup_once(&self) -> Result<LogCleanupStats, std::io::Error> {
        let log_path = Path::new(&self.log_dir);

        // 检查目录是否存在
        // 两处判断刻意不合并成单次 metadata：两个分支的日志级别与文案不同（不存在走
        // debug、存在但非目录走 warn），而旧 exists() 对权限错误等也返回 false 从而走
        // debug 分支，合并后无法复刻。多一次 syscall 在周期清理任务里可忽略。
        if !fs::try_exists(log_path).await.unwrap_or(false) {
            debug!(
                "[log_cleaner] Log directory does not exist, skipping cleanup: {}",
                self.log_dir
            );
            return Ok(LogCleanupStats::default());
        }

        // 检查是否是目录
        if !fs::metadata(log_path)
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            warn!(
                "[log_cleaner] Path is not a directory, skip cleanup: {}",
                self.log_dir
            );
            return Ok(LogCleanupStats::default());
        }

        info!(
            "[log_cleaner] Starting log directory cleanup: {}, retention: {} days",
            self.log_dir,
            self.retention_duration.as_secs() / SECS_PER_DAY
        );

        let mut stats = LogCleanupStats::default();
        let cutoff_time = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(duration) => duration.as_secs(),
            Err(e) => {
                warn!("[log_cleaner] Invalid timestamp, skip cleanup: {}", e);
                return Ok(LogCleanupStats::default());
            }
        };
        let cutoff_time = cutoff_time.saturating_sub(self.retention_duration.as_secs());

        // 读取目录内容
        let mut entries = match fs::read_dir(log_path).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("[log_cleaner] Failed to read directory: {}", e);
                return Ok(LogCleanupStats::default());
            }
        };

        let mut expired_dirs = Vec::new();

        // 遍历目录中的文件和子目录
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let metadata = match entry.metadata().await {
                Ok(m) => m,
                Err(e) => {
                    debug!("[log_cleaner] get file failed: {:?} - {}", path, e);
                    continue;
                }
            };

            // 获取修改时间（更可靠，所有文件系统都支持）
            let modified = match metadata.modified() {
                Ok(time) => time,
                Err(e) => {
                    debug!("[log_cleaner] get modified time failed: {:?} - {}", path, e);
                    continue;
                }
            };

            // 转换为 Unix 时间戳
            let modified_secs = match modified.duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => duration.as_secs(),
                Err(_) => {
                    debug!("[log_cleaner] skip: {:?}", path);
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
                                "[log_cleaner] Deleting expired file: {:?} ({:.2} MB)",
                                path,
                                file_size as f64 / 1024.0 / 1024.0
                            );
                        }
                        Err(e) => {
                            stats.failed_deletions += 1;
                            warn!("[log_cleaner] Failed to delete file: {:?} - {}", path, e);
                        }
                    }
                } else if metadata.is_dir() {
                    expired_dirs.push(path);
                }
            }
        }

        self.reclaim_expired_dirs(expired_dirs, cutoff_time, &mut stats)
            .await;

        if stats.files_deleted > 0 || stats.dirs_deleted > 0 {
            info!(
                "[log_cleaner] Log cleanup completed: deleted {} files, {} dirs, freed {:.2} MB",
                stats.files_deleted,
                stats.dirs_deleted,
                stats.bytes_freed as f64 / 1024.0 / 1024.0
            );
        } else {
            info!("[log_cleaner] Cleanup completed");
        }

        Ok(stats)
    }

    async fn reclaim_expired_dirs(
        &self,
        candidates: Vec<PathBuf>,
        cutoff: u64,
        stats: &mut LogCleanupStats,
    ) {
        if candidates.is_empty() {
            return;
        }
        // The inventory and deletion share a boundary with Docker mount
        // preparation and restore. Defer cleanup while either is in progress.
        let Some(_guard) = docker_manager::try_begin_log_mount_cleanup() else {
            stats.dirs_skipped_unconfirmed += candidates.len() as u64;
            return;
        };
        let mounted = match self
            .mounted_paths
            .mounted_log_directories(&candidates)
            .await
        {
            Ok(inventory) => inventory,
            Err(error) => {
                stats.dirs_skipped_unconfirmed += candidates.len() as u64;
                warn!(%error, "[log_cleaner] Cannot confirm log directory mounts");
                return;
            }
        };

        for path in candidates {
            if mounted.contains(&path) {
                stats.dirs_skipped_in_use += 1;
                continue;
            }
            let still_expired = fs::symlink_metadata(&path).await.is_ok_and(|metadata| {
                metadata.is_dir()
                    && metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .is_some_and(|age| age.as_secs() < cutoff)
            });
            if !still_expired {
                stats.dirs_skipped_unconfirmed += 1;
                continue;
            }
            match fs::remove_dir_all(&path).await {
                Ok(()) => {
                    stats.dirs_deleted += 1;
                    debug!(?path, "[log_cleaner] Deleted expired unmounted directory");
                }
                Err(error) => {
                    stats.failed_deletions += 1;
                    warn!(?path, %error, "[log_cleaner] Failed to delete directory");
                }
            }
        }
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
    /// 已过期但仍被容器挂载、因此保留的目录数量（正常现象，不是错误）
    pub dirs_skipped_in_use: u64,
    /// 已过期但因拿不到挂载清单而保留的目录数量（需要关注）
    pub dirs_skipped_unconfirmed: u64,
    /// 释放的字节数
    pub bytes_freed: u64,
    /// 删除失败的文件/目录数量
    pub failed_deletions: u64,
}

impl LogCleanupStats {
    /// 获取格式化的统计摘要
    pub fn summary(&self) -> String {
        if self.is_empty() {
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
            if self.dirs_skipped_in_use > 0 {
                parts.push(format!(
                    "kept: {} dirs still mounted",
                    self.dirs_skipped_in_use
                ));
            }
            if self.dirs_skipped_unconfirmed > 0 {
                parts.push(format!(
                    "kept: {} dirs without a mount inventory",
                    self.dirs_skipped_unconfirmed
                ));
            }
            if self.failed_deletions > 0 {
                parts.push(format!("failed: {}", self.failed_deletions));
            }
            parts.join(", ")
        }
    }

    /// 本轮既没有删除任何东西，也没有需要报告的保留项
    fn is_empty(&self) -> bool {
        self.files_deleted == 0
            && self.dirs_deleted == 0
            && self.dirs_skipped_in_use == 0
            && self.dirs_skipped_unconfirmed == 0
            && self.failed_deletions == 0
    }
}
