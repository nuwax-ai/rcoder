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
//!
//! # 卡住的候选
//!
//! 候选按顺序复查，复查共用这一轮的截止时间。一个目录的复查如果一直不回来（文件系统
//! 或挂载卡住），每轮的预算都耗在它身上，排在后面的目录永远轮不到；而"顺延"不算故障，
//! 只有 debug 日志，卡住不会被发现。所以给足了时间（[`FAIR_RECHECK_WINDOW`]）还超时的候选
//! 记一轮，连续 [`RECHECK_TIMEOUT_QUARANTINE_SWEEPS`] 轮之后就**隔离**：保留目录、告警、
//! 不再参与后面的轮次，其余目录照常回收。预算本来就快用完时的超时只是没轮到它，不记。
//!
//! 超时记录带过期时间（[`RECHECK_TIMEOUT_MEMORY`]），不长期占着内存：隔离中的目录过了这段
//! 时间，后面的日志清理会再试一次；文件系统恢复了它就被正常回收，仍然卡住就再隔离一轮。
//! 隔离不触发重试，所以"再试一次"是下一次日志清理（默认一天之后）。
//!
//! 隔离在**初次扫描**里就生效：已隔离的目录连第一次 metadata 都不读。复查和初次扫描读的是
//! 同一个目录的同类调用，上一次卡在复查的目录，下一轮很可能连扫描都卡住。

use moka::ops::compute::{CompResult, Op};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tracing::{debug, info, warn};

use super::owner::MountedPathsProbe;

/// 一天的秒数：保留天数换算秒数的乘数
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// 一轮的截止时间：从拿到挂载围栏的写锁那一刻起计，取清单、逐个判定、逐个删除共用它。
///
/// 删除期间一直占着这把写锁，agent 容器创建和 builder 恢复都在锁外等。没有截止时间的话，
/// 积压的过期目录越多、Docker 越慢，锁被占得越久。到期之后不再开始下一步：
/// - 判定还没回来：这一轮所有候选都保留，放锁；
/// - 判定回来了：不再发起删除，也不再复查余下的候选，余下的留到下一轮。
///
/// 它是软预算：不会中途放弃进行中的删除——放弃之后底层可能还在删，锁却已经放开了。所以
/// 实际持锁时间会比它多出最后一次删除所花的时间。
const DIR_RECLAIM_BUDGET: Duration = Duration::from_secs(2);

/// 一轮里最多发起多少次目录删除，失败的也算。时间预算管不住"每个都很快、合起来却很多"
/// 的批次，数量另设上限，余下的留到下一轮。
const DIR_RECLAIM_BATCH: usize = 64;

/// 复查开始时剩余时间不少于这个值，超时才算这个候选自己慢。剩余时间更少时的超时只是
/// 预算快用完、没轮到它，记在它头上会冤枉排在后面的目录
const FAIR_RECHECK_WINDOW: Duration = Duration::from_millis(500);

/// 连续这么多轮复查都慢的候选被隔离
const RECHECK_TIMEOUT_QUARANTINE_SWEEPS: u32 = 3;

/// 一条超时记录从最后一次更新起保留这么久。两轮重试之间隔得比它久，计数从头开始
const RECHECK_TIMEOUT_MEMORY: Duration = Duration::from_secs(60 * 60);

/// 最多记住多少个目录的超时记录，防止异常情况下无限增长
const RECHECK_TIMEOUT_MAX_ENTRIES: u64 = 10_000;

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
    /// 一轮里最多发起的目录删除次数，见 [`DIR_RECLAIM_BATCH`]
    reclaim_batch: usize,
    /// 复查开始时至少要有多少剩余时间，超时才记在候选头上，见 [`FAIR_RECHECK_WINDOW`]
    fair_recheck_window: Duration,
    /// 每个候选连续复查超时的轮数，带过期时间。见模块说明"卡住的候选"
    recheck_timeouts: moka::sync::Cache<PathBuf, u32>,
    /// 让每一次目录删除都失败而不真的删除。测试用：验证失败的删除同样占用每轮的名额
    #[cfg(test)]
    fail_deletions: bool,
    /// 复查之前先等这么久（毫秒）。测试用：模拟卡住的文件系统，中途可以改
    #[cfg(test)]
    recheck_delay_ms: std::sync::atomic::AtomicU64,
    /// 初次扫描读每个条目的 metadata 之前先等这么久（毫秒）。测试用
    #[cfg(test)]
    scan_delay_ms: std::sync::atomic::AtomicU64,
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
            fair_recheck_window: FAIR_RECHECK_WINDOW,
            recheck_timeouts: Self::recheck_timeouts(RECHECK_TIMEOUT_MEMORY),
            #[cfg(test)]
            fail_deletions: false,
            #[cfg(test)]
            recheck_delay_ms: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            scan_delay_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn recheck_timeouts(memory: Duration) -> moka::sync::Cache<PathBuf, u32> {
        moka::sync::Cache::builder()
            .time_to_live(memory)
            .max_capacity(RECHECK_TIMEOUT_MAX_ENTRIES)
            .build()
    }

    /// 把一轮的删除时间预算改成 `budget`。测试用：零预算可以确定性地验证"到期即顺延"
    #[cfg(test)]
    pub(super) fn with_reclaim_budget(mut self, budget: Duration) -> Self {
        self.reclaim_budget = budget;
        self
    }

    #[cfg(test)]
    pub(super) fn with_recheck_delay(self, delay: Duration) -> Self {
        self.set_recheck_delay(delay);
        self
    }

    /// 测试用：中途改变复查前的等待，模拟文件系统卡住之后恢复
    #[cfg(test)]
    pub(super) fn set_recheck_delay(&self, delay: Duration) {
        self.recheck_delay_ms.store(
            delay.as_millis() as u64,
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    /// 测试用：初次扫描读 metadata 之前先等 `delay`，模拟卡住的文件系统
    #[cfg(test)]
    pub(in crate::cleanup_task) fn set_scan_delay(&self, delay: Duration) {
        self.scan_delay_ms.store(
            delay.as_millis() as u64,
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    /// 把"算这个候选自己慢"的剩余时间门槛改成 `window`。测试用：缩短用例耗时
    #[cfg(test)]
    pub(super) fn with_fair_recheck_window(mut self, window: Duration) -> Self {
        self.fair_recheck_window = window;
        self
    }

    /// 把超时记录的保留时长改成 `memory`。测试用：验证记录过期后目录会被重新尝试
    #[cfg(test)]
    pub(super) fn with_recheck_timeout_memory(mut self, memory: Duration) -> Self {
        self.recheck_timeouts = Self::recheck_timeouts(memory);
        self
    }

    /// 把一轮最多删除的目录数改成 `batch`。测试用
    #[cfg(test)]
    pub(super) fn with_reclaim_batch(mut self, batch: usize) -> Self {
        self.reclaim_batch = batch;
        self
    }

    /// 让每一次目录删除都失败而不真的删除。测试用
    #[cfg(test)]
    pub(super) fn failing_deletions(mut self) -> Self {
        self.fail_deletions = true;
        self
    }

    /// 执行一次日志清理
    ///
    /// # 返回
    /// 返回清理的文件数量、目录数量和释放的字节数
    pub async fn cleanup_once(&self) -> Result<LogCleanupStats, std::io::Error> {
        let log_path = Path::new(&self.log_dir);

        match fs::metadata(log_path).await {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                warn!(
                    ?log_path,
                    "[log_cleaner] Path is not a directory, skip cleanup"
                );
                return Ok(LogCleanupStats::default());
            }
            Err(error) if log_directory_absent(&error) => {
                debug!(?log_path, %error, "[log_cleaner] Log directory does not exist, skipping cleanup");
                return Ok(LogCleanupStats::default());
            }
            Err(error) => return Err(error),
        }

        info!(
            "[log_cleaner] Starting log directory cleanup: {}, retention: {} days",
            self.log_dir,
            self.retention_duration.as_secs() / SECS_PER_DAY
        );

        let mut stats = LogCleanupStats::default();
        let cutoff_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_secs();
        let cutoff_time = cutoff_time.saturating_sub(self.retention_duration.as_secs());

        // 读取目录内容
        let mut entries = match fs::read_dir(log_path).await {
            Ok(entries) => entries,
            Err(error) if log_directory_absent(&error) => return Ok(stats),
            Err(error) => return Err(error),
        };

        let mut expired_dirs = Vec::new();
        let mut quarantined = Vec::new();

        // 遍历目录中的文件和子目录
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            // 已隔离的目录连第一次 metadata 都不读：卡住复查的是同类调用，读它可能连扫描
            // 一起卡住，后面的目录和整个清扫任务都跟着停
            if self.is_quarantined(&path) {
                quarantined.push(path);
                continue;
            }
            #[cfg(test)]
            {
                let delay_ms = self.scan_delay_ms.load(std::sync::atomic::Ordering::SeqCst);
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
            let metadata = match entry.metadata().await {
                Ok(m) => m,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    stats.entry_inspection_errors += 1;
                    debug!("[log_cleaner] get file failed: {:?} - {}", path, e);
                    continue;
                }
            };

            // 获取修改时间（更可靠，所有文件系统都支持）
            let modified = match metadata.modified() {
                Ok(time) => time,
                Err(e) => {
                    stats.entry_inspection_errors += 1;
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

        if !quarantined.is_empty() {
            stats.dirs_quarantined = quarantined.len() as u64;
            warn!(
                count = quarantined.len(),
                paths = %summarize_paths(&quarantined),
                "[log_cleaner] Keeping expired directories whose re-check kept timing out; they stay quarantined until their timeout records expire",
            );
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
            stats.note_deferred(candidates.first().map(PathBuf::as_path));
            return;
        };
        // 截止时间从这一刻开始计：取清单、逐个判定、逐个删除共用它。判定到期还没回来就保留
        // 全部候选、放锁；已经发起的删除不包在超时里，照旧等它结束
        let reclaim_started = std::time::Instant::now();
        let deadline = tokio::time::Instant::from_std(reclaim_started) + self.reclaim_budget;
        let mounts = match tokio::time::timeout_at(
            deadline,
            self.mounted_paths.mounted_log_directories(&candidates),
        )
        .await
        {
            Ok(Ok(mounts)) => mounts,
            Ok(Err(error)) => {
                stats.dirs_skipped_unconfirmed += candidates.len() as u64;
                stats.inventory_unavailable += candidates.len() as u64;
                warn!(
                    %error,
                    "[log_cleaner] Cannot confirm log directory mounts, keeping every expired directory this cycle"
                );
                return;
            }
            Err(_elapsed) => {
                stats.dirs_skipped_unconfirmed += candidates.len() as u64;
                stats.inventory_unavailable += candidates.len() as u64;
                warn!(
                    candidates = candidates.len(),
                    budget_ms = self.reclaim_budget.as_millis() as u64,
                    "[log_cleaner] Log directory mounts were not confirmed within the reclaim budget, keeping every expired directory this cycle"
                );
                return;
            }
        };

        let mut attempts = 0usize;
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
            // 这一批的名额或时间预算已用完：余下的不再做任何 I/O，留到下一轮
            if attempts >= self.reclaim_batch || tokio::time::Instant::now() >= deadline {
                stats.dirs_deferred_by_budget += 1;
                stats.note_deferred(Some(&path));
                continue;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let metadata = match tokio::time::timeout_at(deadline, async {
                #[cfg(test)]
                {
                    let delay_ms = self
                        .recheck_delay_ms
                        .load(std::sync::atomic::Ordering::SeqCst);
                    if delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    }
                }
                fs::symlink_metadata(&path).await
            })
            .await
            {
                Ok(metadata) => metadata,
                Err(_) => {
                    stats.dirs_deferred_by_budget += 1;
                    stats.note_deferred(Some(&path));
                    // 给足了时间还没回来才记在它头上；预算本来就快用完只是没轮到它
                    if remaining >= self.fair_recheck_window
                        && let Some(sweeps) = self.note_recheck_timeout(&path)
                    {
                        warn!(
                            path = %path.display(),
                            sweeps,
                            "[log_cleaner] Re-check of an expired directory timed out on {} consecutive sweeps; keeping it and quarantining it until its timeout record expires",
                            sweeps
                        );
                    }
                    continue;
                }
            };
            self.clear_recheck_timeout(&path);
            let still_expired = match metadata {
                Ok(metadata) => metadata.is_dir() && is_older_than(&metadata, cutoff),
                // 列目录之后目录已经消失：没有东西可删，也不是故障
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    debug!(?path, "[log_cleaner] Expired directory is already gone");
                    continue;
                }
                Err(error) => {
                    stats.dirs_skipped_unconfirmed += 1;
                    stats.recheck_errors += 1;
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
            // 复查也可能很慢：真正发起删除之前再确认一次截止时间
            if tokio::time::Instant::now() >= deadline {
                stats.dirs_deferred_by_budget += 1;
                stats.note_deferred(Some(&path));
                continue;
            }
            #[cfg(test)]
            let removed = if self.fail_deletions {
                Err(std::io::Error::other("fixture: directory deletion refused"))
            } else {
                fs::remove_dir_all(&path).await
            };
            #[cfg(not(test))]
            let removed = fs::remove_dir_all(&path).await;
            // 名额在发起时消耗：失败的删除同样占这一轮的名额
            attempts += 1;
            match removed {
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

    /// 这个目录是否在隔离中。读不会延长记录的寿命：隔离中的目录到了时间一定会被重新尝试
    fn is_quarantined(&self, path: &Path) -> bool {
        self.recheck_timeouts
            .get(path)
            .is_some_and(|sweeps| sweeps >= RECHECK_TIMEOUT_QUARANTINE_SWEEPS)
    }

    /// 记一次复查超时，记录的保留时间从此刻重新起算。刚达到隔离轮数时返回轮数，
    /// 调用方据此告警
    fn note_recheck_timeout(&self, path: &Path) -> Option<u32> {
        let result = self
            .recheck_timeouts
            .entry(path.to_path_buf())
            .and_compute_with(|entry| {
                Op::Put(
                    entry
                        .map_or(0, |entry| entry.into_value())
                        .saturating_add(1),
                )
            });
        match result {
            CompResult::Inserted(entry) | CompResult::ReplacedWith(entry) => {
                let sweeps = entry.into_value();
                (sweeps == RECHECK_TIMEOUT_QUARANTINE_SWEEPS).then_some(sweeps)
            }
            // 本次调用只会 Put：走到这里说明这条记录被别的调用方动了
            _ => None,
        }
    }

    fn clear_recheck_timeout(&self, path: &Path) {
        self.recheck_timeouts.invalidate(path);
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

/// 告警里的路径清单：最多列前 5 个
fn summarize_paths(paths: &[PathBuf]) -> String {
    const SHOWN: usize = 5;
    let shown: Vec<String> = paths
        .iter()
        .take(SHOWN)
        .map(|path| path.display().to_string())
        .collect();
    if paths.len() > SHOWN {
        format!("{} … (+{})", shown.join(", "), paths.len() - SHOWN)
    } else {
        shown.join(", ")
    }
}

fn log_directory_absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
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
    /// 挂载清单拿不到（查询失败或超时）而保留的候选数量。短暂失败，下一轮重试
    pub inventory_unavailable: u64,
    /// 复查候选时出错的次数。短暂失败，下一轮重试
    pub recheck_errors: u64,
    /// 初次扫描时无法读取条目或修改时间的数量
    pub entry_inspection_errors: u64,
    /// 连续复查超时而被隔离、本轮不再处理的目录数量（需要关注）
    pub dirs_quarantined: u64,
    /// 本轮第一个被顺延的目录（争锁、预算用完或复查超时），告警时用来定位
    pub first_deferred: Option<PathBuf>,
}

impl LogCleanupStats {
    pub(crate) fn has_errors(&self) -> bool {
        self.dirs_skipped_unconfirmed > 0
            || self.failed_deletions > 0
            || self.entry_inspection_errors > 0
            || self.dirs_quarantined > 0
    }

    /// 记下本轮第一个被顺延的目录
    fn note_deferred(&mut self, path: Option<&Path>) {
        if self.first_deferred.is_none()
            && let Some(path) = path
        {
            self.first_deferred = Some(path.to_path_buf());
        }
    }

    /// 这一轮有没有"本该做完却没做完"、值得下一轮清扫接着做的事：清单拿不到、复查出错、
    /// 删除失败、条目读取失败、争锁顺延、批次或时间预算顺延。
    ///
    /// 不含"挂载源无法确认指向哪里"和"目录在扫描后变了"：前者是持续状态（只要那个容器还在，
    /// 每轮都会得到同样的结论，重试不会变得可确认），后者是正常竞态。它们只计数、照常保留，
    /// 不重置日志清理的周期。
    pub(crate) fn needs_retry(&self) -> bool {
        self.inventory_unavailable > 0
            || self.recheck_errors > 0
            || self.failed_deletions > 0
            || self.entry_inspection_errors > 0
            || self.dirs_deferred_while_mounting > 0
            || self.dirs_deferred_by_budget > 0
    }

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
            if self.entry_inspection_errors > 0 {
                parts.push(format!(
                    "uninspected: {} entries",
                    self.entry_inspection_errors
                ));
            }
            if self.dirs_quarantined > 0 {
                parts.push(format!(
                    "quarantined: {} dirs (re-check timed out {} sweeps in a row)",
                    self.dirs_quarantined, RECHECK_TIMEOUT_QUARANTINE_SWEEPS
                ));
            }
            parts.join(", ")
        }
    }

    /// 本轮既没有删除任何东西，也没有需要报告的保留或顺延项
    fn is_empty(&self) -> bool {
        self.files_deleted == 0
            && self.dirs_deleted == 0
            && self.dirs_skipped_in_use == 0
            && self.dirs_quarantined == 0
            && !self.needs_retry()
    }
}
