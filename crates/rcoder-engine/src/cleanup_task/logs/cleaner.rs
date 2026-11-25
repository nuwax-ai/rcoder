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
//! # 保留、顺延与告警
//!
//! - **保留并告警**才需要人看：整份清单拿不到；某个候选无法确认是否仍被挂载；删除失败。
//!   没有任何路径会把"查不到"当成"没有挂载"。
//! - **顺延**不是故障，下一轮继续：取锁时正好有挂载准备在进行；或者本轮删除目录的
//!   预算用完（时间或数量）。两者分别计数，不告警。
//!
//! 单个候选判不出来只影响它自己：其余能判定的目录照常回收。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tracing::{debug, info, warn};

use super::owner::MountedPathsProbe;

/// 一天的秒数：保留天数换算秒数的乘数
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// 一轮里删除目录的时间预算：从开始判定起计，到期后不再发起删除，余下的留到下一轮。
///
/// 删除期间一直占着挂载围栏的写锁，agent 容器创建和 builder 恢复都在锁外等。没有预算
/// 的话，积压的过期目录越多，锁被占得越久。预算只决定"要不要发起下一次删除"，不会
/// 中途放弃进行中的删除——放弃之后底层可能还在删，锁却已经放开了。
const DIR_RECLAIM_BUDGET: Duration = Duration::from_secs(2);

/// 一轮里最多删除的目录数。时间预算管不住"每个都很快、合起来却很多"的批次，数量另设
/// 上限，余下的留到下一轮。
const DIR_RECLAIM_BATCH: usize = 64;

/// 日志清理器
pub struct LogCleaner {
    /// 日志目录路径
    log_dir: String,
    /// 日志保留时长
    retention_duration: Duration,
    /// 当前被容器挂载的宿主机路径清单
    mounted_paths: Arc<dyn MountedPathsProbe>,
    /// 一轮里删除目录的时间预算，见 [`DIR_RECLAIM_BUDGET`]
    reclaim_budget: Duration,
    /// 一轮里最多删除的目录数，见 [`DIR_RECLAIM_BATCH`]
    reclaim_batch: usize,
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
            reclaim_budget: DIR_RECLAIM_BUDGET,
            reclaim_batch: DIR_RECLAIM_BATCH,
        }
    }

    /// 把一轮的删除时间预算改成 `budget`。测试用：零预算可以确定性地验证"到期即顺延"
    #[cfg(test)]
    pub(super) fn with_reclaim_budget(mut self, budget: Duration) -> Self {
        self.reclaim_budget = budget;
        self
    }

    /// 把一轮最多删除的目录数改成 `batch`。测试用
    #[cfg(test)]
    pub(super) fn with_reclaim_batch(mut self, batch: usize) -> Self {
        self.reclaim_batch = batch;
        self
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
        // 取清单到删除的这一段与挂载准备、builder 恢复互斥。对方正在进行属于正常现象：
        // 本轮顺延，单独计数，不告警。
        let Some(_guard) = docker_manager::try_begin_log_mount_cleanup() else {
            stats.dirs_deferred_while_mounting += candidates.len() as u64;
            return;
        };
        // 预算从这一刻开始计：接下来取清单、逐个复查的时间同样占着写锁
        let reclaim_started = std::time::Instant::now();
        let mounts = match self
            .mounted_paths
            .mounted_log_directories(&candidates)
            .await
        {
            Ok(mounts) => mounts,
            Err(error) => {
                stats.dirs_skipped_unconfirmed += candidates.len() as u64;
                warn!(
                    %error,
                    "[log_cleaner] Cannot confirm log directory mounts, keeping every expired directory this cycle"
                );
                return;
            }
        };

        let mut reclaimed = 0usize;
        for path in candidates {
            if mounts.mounted.contains(&path) {
                stats.dirs_skipped_in_use += 1;
                continue;
            }
            if let Some(reason) = mounts.unconfirmed_reason(&path) {
                stats.dirs_skipped_unconfirmed += 1;
                warn!(
                    candidate = %path.display(),
                    %reason,
                    "[log_cleaner] Cannot confirm whether an expired log directory is still mounted, keeping it"
                );
                continue;
            }
            // 这一批的额度已用完：余下的不再复查，留到下一轮
            if reclaimed >= self.reclaim_batch {
                stats.dirs_deferred_by_budget += 1;
                continue;
            }
            let still_expired = match fs::symlink_metadata(&path).await {
                Ok(metadata) => metadata.is_dir() && is_older_than(&metadata, cutoff),
                // 列目录之后目录已经消失：没有东西可删，也不是故障
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    debug!(?path, "[log_cleaner] Expired directory is already gone");
                    continue;
                }
                Err(error) => {
                    stats.dirs_skipped_unconfirmed += 1;
                    warn!(?path, %error, "[log_cleaner] Cannot re-check an expired directory, keeping it");
                    continue;
                }
            };
            if !still_expired {
                stats.dirs_skipped_unconfirmed += 1;
                warn!(
                    ?path,
                    "[log_cleaner] Expired directory changed since the scan, keeping it"
                );
                continue;
            }
            // 复查也可能很慢：真正发起删除之前再确认一次时间预算
            if reclaim_started.elapsed() >= self.reclaim_budget {
                stats.dirs_deferred_by_budget += 1;
                continue;
            }
            match fs::remove_dir_all(&path).await {
                Ok(()) => {
                    stats.dirs_deleted += 1;
                    reclaimed += 1;
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

/// 修改时间是否早于 `cutoff`（Unix 秒）；取不到修改时间视为未过期
fn is_older_than(metadata: &std::fs::Metadata, cutoff: u64) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .is_some_and(|modified| modified.as_secs() < cutoff)
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
    /// 已过期但无法确认是否仍被挂载、因此保留的目录数量（需要关注）
    pub dirs_skipped_unconfirmed: u64,
    /// 因挂载准备正在进行而顺延到下一轮的目录数量（正常现象）
    pub dirs_deferred_while_mounting: u64,
    /// 因本轮的删除预算用完（时间或数量）而顺延到下一轮的目录数量（正常现象）
    pub dirs_deferred_by_budget: u64,
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
                    "kept: {} dirs without a confirmed mount status",
                    self.dirs_skipped_unconfirmed
                ));
            }
            if self.dirs_deferred_while_mounting > 0 {
                parts.push(format!(
                    "deferred: {} dirs (mount preparation in progress)",
                    self.dirs_deferred_while_mounting
                ));
            }
            if self.dirs_deferred_by_budget > 0 {
                parts.push(format!(
                    "deferred: {} dirs (reclaim budget spent)",
                    self.dirs_deferred_by_budget
                ));
            }
            if self.failed_deletions > 0 {
                parts.push(format!("failed: {}", self.failed_deletions));
            }
            parts.join(", ")
        }
    }

    /// 本轮既没有删除任何东西，也没有需要报告的保留或顺延项
    fn is_empty(&self) -> bool {
        self.files_deleted == 0
            && self.dirs_deleted == 0
            && self.dirs_skipped_in_use == 0
            && self.dirs_skipped_unconfirmed == 0
            && self.dirs_deferred_while_mounting == 0
            && self.dirs_deferred_by_budget == 0
            && self.failed_deletions == 0
    }
}
