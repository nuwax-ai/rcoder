//! 日志清理器：删除过期的日志文件和过期的日志目录。
//!
//! # 删除判据
//!
//! 目录的修改时间只用来判断"是否已超过保留期"，**不单独决定删除**。一个目录会被
//! `remove_dir_all` 删掉，必须同时满足：
//!
//! 1. 修改时间早于保留期（保留期有下限 [`MIN_RETENTION`]，刚建出来的目录不会是候选）；
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
//! # 为什么不用和挂载准备互斥
//!
//! 清理器手里的候选都是旧目录。正在准备挂载的容器绑定的都是刚建的新目录——创建容器和
//! builder 重启都独占新建，不复用已有的目录（`docker_manager::log_directory`）——从建出来
//! 到 Docker 登记这个挂载，它一直登记为"正在准备挂载"，运行时的挂载清单把登记着的目录算作
//! 仍被挂载。还在用旧目录的只有已经存在的容器，它们就在清单里。所以"查清单 + 删之前再确认
//! 一次"就够了，不需要一把全局锁。
//!
//! 以前这里有一把全局锁：挂载准备拿读锁，清理拿写锁。它护住的是 builder 重启重放旧日志
//! 目录的那段窗口，代价是一次卡住的删除会把锁占住，所有 agent 容器启动、所有 builder 重启
//! 跟着一起等。
//!
//! # 保留、顺延与告警
//!
//! - **保留并告警**才需要人看：整份清单拿不到；某个候选无法确认是否仍被挂载；删除失败。
//!   没有任何路径会把"查不到"当成"没有挂载"。
//! - **顺延**不是故障，下一轮继续：本轮删除目录的预算用完（时间或数量）。只计数，不告警。
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
//! 初次扫描里也有同类的事：一个条目的 metadata（或删一个过期文件）一直不回来。只靠扫描的
//! 总时限的话，每次扫描都卡在它身上、整次作废，回收要等扫描完成才开始，整棵日志树就不再
//! 回收，每轮还多搭进去一个阻塞线程。所以单个条目也有时限（[`SCAN_ENTRY_TIMEOUT`]）：
//! 超时就记一轮（和复查超时同一张表）、跳过它，继续扫后面的；连续几轮之后同样隔离。
//!
//! 超时记录带过期时间（[`RECHECK_TIMEOUT_MEMORY`]），不长期占着内存：隔离中的目录过了这段
//! 时间，后面的日志清理会再试一次；文件系统恢复了它就被正常回收，仍然卡住就再隔离一轮。
//! 隔离不触发重试，所以"再试一次"是下一次日志清理（默认一天之后）。
//!
//! 隔离在**初次扫描**里就生效：已隔离的目录连第一次 metadata 都不读。复查和初次扫描读的是
//! 同一个目录的同类调用，上一次卡在复查的目录，下一轮很可能连扫描都卡住。
//!
//! # 扫描的时限
//!
//! 初次扫描整体限时（[`SCAN_TIMEOUT`]）：读目录、读每个条目的 metadata 都没有逐项的截止
//! 时间。总时限只在日志根自身卡住时才会触发（读根目录的 metadata、列目录）：不设上限会
//! 把这个任务永远停在这里。时限只管扫描，已经发起的目录删除照旧等到结束：中途放弃的话
//! 底层可能还在删，这个任务不能同时再起一轮。单个条目卡住由逐项的时限处理，见下文。
//!
//! 一轮的统计见 [`LogCleanupStats`]。

use moka::ops::compute::{CompResult, Op};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tracing::{debug, info, warn};

use super::owner::MountedPathsProbe;
use super::stats::LogCleanupStats;

/// 一天的秒数：保留天数换算秒数的乘数
const SECS_PER_DAY: u64 = 24 * 60 * 60;

/// 保留期下限：配置的保留期比它短时按它计。
///
/// 容器的日志目录是新建的（`docker_manager` 的 `log_directory`），所以清理器不必和挂载准备
/// 互斥：手里的候选都是旧目录。正在准备挂载的目录有两层保护：建出来到登记为"正在准备挂载"
/// 之间的那一瞬间靠这个下限（配成 0 天时它也算过期）；登记之后到创建结果确定之间，可能隔着
/// 一次很慢的镜像拉取，靠登记本身（运行时的挂载清单把登记着的目录算作仍被挂载；创建结果
/// 不明的，登记保留到过期）。按保留下限算，停掉的容器的日志最多晚这么久回收。
const MIN_RETENTION: Duration = Duration::from_secs(10 * 60);

/// 一轮回收的截止时间：从开始取清单起计，取清单、逐个判定、逐个删除共用它。
///
/// 没有截止时间的话，积压的过期目录越多、Docker 越慢，一轮拖得越久，日志清理迟迟不结束。
/// 到期之后不再开始下一步：
/// - 判定还没回来：这一轮所有候选都保留；
/// - 判定回来了：不再发起删除，也不再复查余下的候选，余下的留到下一轮。
///
/// 它是软预算：不会中途放弃进行中的删除——放弃之后底层可能还在删。所以一轮实际花的时间
/// 会比它多出最后一次删除所花的时间。
const DIR_RECLAIM_BUDGET: Duration = Duration::from_secs(2);

/// 一轮里最多发起多少次目录删除，失败的也算。时间预算管不住"每个都很快、合起来却很多"
/// 的批次，数量另设上限，余下的留到下一轮。
const DIR_RECLAIM_BATCH: usize = 64;

/// 复查开始时剩余时间不少于这个值，超时才算这个候选自己慢。剩余时间更少时的超时只是
/// 预算快用完、没轮到它，记在它头上会冤枉排在后面的目录
const FAIR_RECHECK_WINDOW: Duration = Duration::from_millis(500);

/// 连续这么多轮复查都慢的候选被隔离
pub(super) const RECHECK_TIMEOUT_QUARANTINE_SWEEPS: u32 = 3;

/// 一条超时记录从最后一次更新起保留这么久。两轮重试之间隔得比它久，计数从头开始
const RECHECK_TIMEOUT_MEMORY: Duration = Duration::from_secs(60 * 60);

/// 最多记住多少个目录的超时记录，防止异常情况下无限增长
const RECHECK_TIMEOUT_MAX_ENTRIES: u64 = 10_000;

/// 初次扫描一轮的时间上限，见 [`LogCleaner::cleanup_once`]。正常一轮远用不了这么久
const SCAN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// 初次扫描里读一个条目（或删一个过期文件）的时限。超过了就当它卡住：记一轮超时、跳过
/// 它，继续扫后面的。被跳过的条目计入 [`LogCleanupStats::entries_stuck_in_scan`]，下一轮
/// 清扫（不是下一个完整周期）会再试；连续 [`RECHECK_TIMEOUT_QUARANTINE_SWEEPS`] 轮之后隔离
const SCAN_ENTRY_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// 初次扫描一轮的时间上限，见 [`SCAN_TIMEOUT`]
    scan_timeout: Duration,
    /// 初次扫描里读单个条目的时限，见 [`SCAN_ENTRY_TIMEOUT`]
    scan_entry_timeout: Duration,
    /// 测试用：让扫描卡在这几个条目上（读 metadata 一直不返回）
    #[cfg(test)]
    scan_stalled: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
    /// 测试用：让删这几个过期文件一直不返回
    #[cfg(test)]
    removal_stalled: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
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
    /// 发起目录删除之前先等这么久（毫秒）。测试用
    #[cfg(test)]
    delete_delay_ms: std::sync::atomic::AtomicU64,
}

impl LogCleaner {
    /// 创建新的日志清理器
    ///
    /// `retention_days` 来自配置文件（`cleanup_config.log_cleanup.log_retention_days`），
    /// 是不受信任的输入：换算成秒时必须用 `saturating_mul`。直接相乘在 debug 构建下会
    /// panic，在 release 构建下会静默回绕——例如 `144115188075855872` 天会回绕成 0 秒。
    /// 不足 [`MIN_RETENTION`] 的保留期按它计。
    pub fn new(
        log_dir: impl Into<String>,
        retention_days: u64,
        mounted_paths: Arc<dyn MountedPathsProbe>,
    ) -> Self {
        Self {
            log_dir: log_dir.into(),
            retention_duration: Duration::from_secs(retention_days.saturating_mul(SECS_PER_DAY))
                .max(MIN_RETENTION),
            mounted_paths,
            reclaim_budget: DIR_RECLAIM_BUDGET,
            reclaim_batch: DIR_RECLAIM_BATCH,
            fair_recheck_window: FAIR_RECHECK_WINDOW,
            scan_timeout: SCAN_TIMEOUT,
            scan_entry_timeout: SCAN_ENTRY_TIMEOUT,
            #[cfg(test)]
            scan_stalled: std::sync::Mutex::new(std::collections::HashSet::new()),
            #[cfg(test)]
            removal_stalled: std::sync::Mutex::new(std::collections::HashSet::new()),
            recheck_timeouts: Self::recheck_timeouts(RECHECK_TIMEOUT_MEMORY),
            #[cfg(test)]
            fail_deletions: false,
            #[cfg(test)]
            recheck_delay_ms: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            scan_delay_ms: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            delete_delay_ms: std::sync::atomic::AtomicU64::new(0),
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

    /// 测试用：缩短一轮初次扫描的时间上限
    #[cfg(test)]
    pub(in crate::cleanup_task) fn set_scan_timeout(&mut self, timeout: Duration) {
        self.scan_timeout = timeout;
    }

    /// 测试用：缩短扫描里单个条目的时限
    #[cfg(test)]
    pub(in crate::cleanup_task) fn set_scan_entry_timeout(&mut self, timeout: Duration) {
        self.scan_entry_timeout = timeout;
    }

    /// 测试用：让扫描卡在 `path` 上——读它的 metadata 一直不返回
    #[cfg(test)]
    pub(in crate::cleanup_task) fn stall_scan_of(&self, path: &Path) {
        self.scan_stalled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.to_path_buf());
    }

    /// 测试用：让删过期文件 `path` 一直不返回
    #[cfg(test)]
    pub(in crate::cleanup_task) fn stall_scan_removal_of(&self, path: &Path) {
        self.removal_stalled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path.to_path_buf());
    }

    /// 测试用：文件系统恢复，扫描和删除不再卡住
    #[cfg(test)]
    pub(in crate::cleanup_task) fn unstall_scan(&self) {
        self.scan_stalled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.removal_stalled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// 测试用：发起目录删除之前先等 `delay`，模拟很慢的删除
    #[cfg(test)]
    pub(in crate::cleanup_task) fn set_delete_delay(&self, delay: Duration) {
        self.delete_delay_ms.store(
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
    /// 返回清理的文件数量、目录数量和释放的字节数；扫描超过 [`SCAN_TIMEOUT`] 时返回
    /// [`LogCleanupError::ScanTimedOut`]，这一轮什么都没回收
    pub(in crate::cleanup_task) async fn cleanup_once(
        &self,
    ) -> Result<LogCleanupStats, LogCleanupError> {
        let log_path = Path::new(&self.log_dir);
        let mut stats = LogCleanupStats::default();
        let cutoff_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(std::io::Error::other)?
            .as_secs();
        let cutoff_time = cutoff_time.saturating_sub(self.retention_duration.as_secs());

        // 扫描整体限时：日志根自身卡住（读根目录的 metadata、列目录）时不设上限会把清扫任务
        // 一起停掉。单个条目卡住由 SCAN_ENTRY_TIMEOUT 处理，见 inspect_entry。只管扫描——
        // 已经发起的目录删除在 reclaim_expired_dirs 里，照旧等到它结束（见那里的说明）
        let scanned = tokio::time::timeout(self.scan_timeout, async {
        match fs::metadata(log_path).await {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                warn!(
                    ?log_path,
                    "[log_cleaner] Path is not a directory, skip cleanup"
                );
                return Ok(None);
            }
            Err(error) if log_directory_absent(&error) => {
                debug!(?log_path, %error, "[log_cleaner] Log directory does not exist, skipping cleanup");
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }

        info!(
            "[log_cleaner] Starting log directory cleanup: {}, retention: {:?}",
            self.log_dir, self.retention_duration
        );

        // 读取目录内容
        let mut entries = match fs::read_dir(log_path).await {
            Ok(entries) => entries,
            Err(error) if log_directory_absent(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
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
            let metadata = match self.inspect_entry(&entry).await {
                Ok(Ok(m)) => m,
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Ok(Err(e)) => {
                    stats.entry_inspection_errors += 1;
                    debug!("[log_cleaner] get file failed: {:?} - {}", path, e);
                    continue;
                }
                Err(_elapsed) => {
                    self.note_scan_stuck(&path, &mut stats);
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

                    match self.remove_expired_file(&path).await {
                        Ok(Ok(_)) => {
                            stats.files_deleted += 1;
                            stats.bytes_freed += file_size;
                            debug!(
                                "[log_cleaner] Deleting expired file: {:?} ({:.2} MB)",
                                path,
                                file_size as f64 / 1024.0 / 1024.0
                            );
                        }
                        Ok(Err(e)) => {
                            stats.failed_deletions += 1;
                            warn!("[log_cleaner] Failed to delete file: {:?} - {}", path, e);
                        }
                        Err(_elapsed) => self.note_scan_stuck(&path, &mut stats),
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
        Ok(Some(expired_dirs))
        })
        .await
        .unwrap_or(Err(LogCleanupError::ScanTimedOut(self.scan_timeout)))?;
        let Some(expired_dirs) = scanned else {
            return Ok(stats);
        };

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
        // 截止时间从这一刻开始计：取清单、逐个判定、逐个删除共用它。判定到期还没回来就保留
        // 全部候选；已经发起的删除不包在超时里，照旧等它结束
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
            {
                let delay_ms = self
                    .delete_delay_ms
                    .load(std::sync::atomic::Ordering::SeqCst);
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
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

    /// 读一个条目的 metadata，带逐项的时限。测试里先等 `scan_delay`，并且卡在登记过的条目上
    async fn inspect_entry(
        &self,
        entry: &fs::DirEntry,
    ) -> Result<std::io::Result<std::fs::Metadata>, tokio::time::error::Elapsed> {
        tokio::time::timeout(self.scan_entry_timeout, async {
            #[cfg(test)]
            {
                let delay_ms = self.scan_delay_ms.load(std::sync::atomic::Ordering::SeqCst);
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                if self
                    .scan_stalled
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains(&entry.path())
                {
                    std::future::pending::<()>().await;
                }
            }
            entry.metadata().await
        })
        .await
    }

    /// 删一个过期文件，带逐项的时限
    async fn remove_expired_file(
        &self,
        path: &Path,
    ) -> Result<std::io::Result<()>, tokio::time::error::Elapsed> {
        tokio::time::timeout(self.scan_entry_timeout, async {
            #[cfg(test)]
            if self
                .removal_stalled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(path)
            {
                std::future::pending::<()>().await;
            }
            fs::remove_file(path).await
        })
        .await
    }

    /// 初次扫描里一个条目一直读不回来（或删不回来）：记一轮超时（和复查超时同一张表），
    /// 本轮跳过它
    fn note_scan_stuck(&self, path: &Path, stats: &mut LogCleanupStats) {
        stats.entries_stuck_in_scan += 1;
        stats.note_deferred(Some(path));
        if let Some(sweeps) = self.note_recheck_timeout(path) {
            warn!(
                path = %path.display(),
                sweeps,
                "[log_cleaner] Reading this entry of the log directory timed out on {} consecutive sweeps; keeping it and quarantining it until its timeout record expires",
                sweeps
            );
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

/// 一轮日志清理没有正常完成的原因
#[derive(Debug)]
pub(in crate::cleanup_task) enum LogCleanupError {
    Io(std::io::Error),
    /// 初次扫描超过了 [`SCAN_TIMEOUT`]。底层还没返回的阻塞调用仍占着线程，调用方不该
    /// 很快再试
    ScanTimedOut(Duration),
}

impl std::fmt::Display for LogCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::ScanTimedOut(timeout) => write!(
                f,
                "scanning the log directory did not finish within {timeout:?}"
            ),
        }
    }
}

impl std::error::Error for LogCleanupError {}

impl From<std::io::Error> for LogCleanupError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
