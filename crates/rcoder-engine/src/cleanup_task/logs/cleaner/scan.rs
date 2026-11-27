//! 初次扫描：列日志根的第一层，读每个条目的 metadata，认出已经过期的目录和文件。扫描里
//! **只读不删**，删除在 [`LogCleaner::reclaim_expired_dirs`] 和
//! [`LogCleaner::delete_expired_files`]。
//!
//! 整个扫描有限时（[`LogCleaner::cleanup_once`]），单个条目也有限时
//! （[`SCAN_ENTRY_TIMEOUT`]）：一个条目的 metadata 一直读不回来，记一轮、跳过它，继续扫
//! 后面的。连续几轮之后隔离，见模块说明"卡住的候选"。

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::fs;
use tracing::{debug, info, warn};

use super::{LogCleaner, LogCleanupStats, log_directory_absent};

/// 初次扫描里读一个条目的 metadata 的时限。超过了就当它卡住：记一轮超时、跳过它，继续
/// 扫后面的。被跳过的条目计入 [`LogCleanupStats::entries_stuck_in_scan`]，下一轮清扫
/// （不是下一个完整周期）会再试；连续 [`super::RECHECK_TIMEOUT_QUARANTINE_SWEEPS`] 轮之后
/// 隔离。
///
/// 它也决定了"多少个卡住的条目会让整个扫描超时"：每个卡住的条目占掉这么久，
/// [`super::SCAN_TIMEOUT`] 里容纳得下几十个。
pub(super) const SCAN_ENTRY_TIMEOUT: Duration = Duration::from_secs(5);

/// 初次扫描认出来的东西
#[derive(Default)]
pub(super) struct Scanned {
    /// 确认已过期的目录，等回收
    pub(super) expired_dirs: Vec<PathBuf>,
    /// 确认已过期的文件，连同扫描时读到的它的大小
    pub(super) expired_files: Vec<(PathBuf, u64)>,
    /// 隔离中、这一轮连 metadata 都没读的目录
    pub(super) quarantined: Vec<PathBuf>,
}

/// 扫描的下场：扫完了，或者日志根根本不存在（或者不是目录）
#[derive(Eq, PartialEq)]
pub(super) enum ScanOutcome {
    Scanned,
    Absent,
}

impl LogCleaner {
    /// 列日志根的第一层，把过期的目录和文件记进 `scanned`；读不回来的条目记进 `stats`
    pub(super) async fn scan(
        &self,
        log_path: &Path,
        cutoff: u64,
        stats: &mut LogCleanupStats,
        scanned: &mut Scanned,
    ) -> io::Result<ScanOutcome> {
        match fs::metadata(log_path).await {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                warn!(
                    ?log_path,
                    "[log_cleaner] Path is not a directory, skip cleanup"
                );
                return Ok(ScanOutcome::Absent);
            }
            Err(error) if log_directory_absent(&error) => {
                debug!(?log_path, %error, "[log_cleaner] Log directory does not exist, skipping cleanup");
                return Ok(ScanOutcome::Absent);
            }
            Err(error) => return Err(error),
        }

        info!(
            "[log_cleaner] Starting log directory cleanup: {}, retention: {:?}",
            self.log_dir, self.retention_duration
        );

        // 读取目录内容
        let mut entries = match fs::read_dir(log_path).await {
            Ok(entries) => entries,
            Err(error) if log_directory_absent(&error) => return Ok(ScanOutcome::Absent),
            Err(error) => return Err(error),
        };

        // 遍历目录中的文件和子目录
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            // 已隔离的目录连第一次 metadata 都不读：卡住复查的是同类调用，读它可能连扫描
            // 一起卡住，后面的目录和整个清扫任务都跟着停
            if self.is_quarantined(&path) {
                scanned.quarantined.push(path);
                continue;
            }
            let metadata = match self.inspect_entry(&entry).await {
                Ok(Ok(m)) => m,
                Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => continue,
                Ok(Err(e)) => {
                    stats.entry_inspection_errors += 1;
                    debug!("[log_cleaner] get file failed: {:?} - {}", path, e);
                    continue;
                }
                Err(_elapsed) => {
                    self.note_scan_stuck(&path, stats);
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
            if modified_secs < cutoff {
                if metadata.is_file() {
                    scanned.expired_files.push((path, metadata.len()));
                } else if metadata.is_dir() {
                    scanned.expired_dirs.push(path);
                }
            }
        }

        if !scanned.quarantined.is_empty() {
            stats.dirs_quarantined = scanned.quarantined.len() as u64;
            warn!(
                count = scanned.quarantined.len(),
                paths = %super::summarize_paths(&scanned.quarantined),
                "[log_cleaner] Keeping expired directories whose re-check kept timing out; they stay quarantined until their timeout records expire",
            );
        }
        Ok(ScanOutcome::Scanned)
    }

    /// 读一个条目的 metadata，带逐项的时限。测试里先等 `scan_delay`，并且卡在登记过的条目上
    async fn inspect_entry(
        &self,
        entry: &fs::DirEntry,
    ) -> Result<io::Result<std::fs::Metadata>, tokio::time::error::Elapsed> {
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

    /// 初次扫描里一个条目一直读不回来：记一轮超时（和复查超时同一张表），本轮跳过它
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

    /// 测试用：文件系统恢复，扫描不再卡住
    #[cfg(test)]
    pub(in crate::cleanup_task) fn unstall_scan(&self) {
        self.scan_stalled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// 测试用：删一个过期文件之前先等 `delay`，模拟很慢的删除
    #[cfg(test)]
    pub(in crate::cleanup_task) fn set_removal_delay(&self, delay: Duration) {
        self.removal_delay_ms.store(
            delay.as_millis() as u64,
            std::sync::atomic::Ordering::SeqCst,
        );
    }
}
