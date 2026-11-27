//! 日志清理任务：自己的循环，不和闲置容器的扫描、回收串在一起。
//!
//! 日志清理有自己的周期（`cleanup_config.log_cleanup.cleanup_interval_seconds`，默认一天），和
//! 闲置容器的扫描、回收是两件互不相干的事。以前它排在清扫任务每一轮的最前面：日志目录卡在
//! 文件系统上（读 metadata 不返回、删除很慢），闲置容器的扫描和回收就跟着停。现在它各跑各的
//! 循环，清扫任务不等它。删什么、怎么判见 [`super::logs`]。
//!
//! # 调度
//!
//! 循环按清扫的间隔（`cleanup_interval_seconds`，默认 5 分钟）醒一次，只检查是否到期：
//!
//! - 从未跑过，或距上一次完成的日志清理已满一个周期：跑一轮；
//! - 这一轮有没做完的事（清单拿不到、复查出错、删除失败、预算用完）：不推进周期，下一次
//!   醒来接着做；
//! - 扫描卡到时间上限被放弃：推进周期，等下一个完整周期再试——底层还没返回的阻塞调用仍
//!   占着线程，不能隔几分钟再搭一个进去。
//!
//! 日志保留期以天计，跟着闲置回收的节奏反复扫描没有意义，所以到期才扫。
//!
//! # 停滞告警
//!
//! 连续 [`STALLED_SWEEPS`] 轮"要重试、却什么都没删掉"时告警，并点名第一个被顺延的目录。
//! 顺延本身只记 debug；没有这个告警，一个复查一直卡住的候选会让清理无声地停在原地。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::time::{Instant, MissedTickBehavior, interval};
use tracing::{debug, info, warn};

use super::config::CleanupConfig;
use super::logs::{LogCleaner, LogCleanupError, MountedPathsProbe, RuntimeMountedPaths};
use container_runtime_api::ContainerRuntime;

/// 连续这么多轮没有进展就告警
const STALLED_SWEEPS: u32 = 3;

/// 检查间隔的下限：`tokio::time::interval` 不接受零
const MIN_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// 日志清理任务
pub(crate) struct LogCleanupTask {
    log_cleaner: LogCleaner,
    /// 多久检查一次是否到期
    check_interval: Duration,
    /// 两次日志清理之间至少隔多久
    cleanup_interval: Duration,
    /// 上一次完成日志清理的时间。没做完的那一轮不算，下一次检查接着做
    last_cleanup: Option<Instant>,
    /// 连续"要重试、却没删掉任何东西"的轮数。有进展或干净完成时清零
    stalled_sweeps: u32,
}

impl LogCleanupTask {
    /// 生产装配：挂载判定走运行时的权威清单，节奏取自清理配置
    pub(crate) fn from_config(config: &CleanupConfig, runtime: Arc<dyn ContainerRuntime>) -> Self {
        Self::new(
            config.log_dir.clone(),
            config.log_retention_duration.as_secs() / 24 / 60 / 60,
            Arc::new(RuntimeMountedPaths::new(runtime)),
            config.cleanup_interval,
            config.log_cleanup_interval,
        )
    }

    /// `check_interval` 是醒来看一眼的间隔（清扫的间隔），`cleanup_interval` 是两次日志清理
    /// 之间至少隔多久
    pub(crate) fn new(
        log_dir: impl Into<String>,
        retention_days: u64,
        mounted_paths: Arc<dyn MountedPathsProbe>,
        check_interval: Duration,
        cleanup_interval: Duration,
    ) -> Self {
        Self {
            log_cleaner: LogCleaner::new(log_dir, retention_days, mounted_paths),
            check_interval: check_interval.max(MIN_CHECK_INTERVAL),
            cleanup_interval,
            last_cleanup: None,
            stalled_sweeps: 0,
        }
    }

    /// 运行到收到 shutdown 信号
    pub(crate) async fn run(&mut self, mut shutdown_rx: broadcast::Receiver<()>) {
        info!("[log_cleanup] log cleanup task started");
        let mut ticker = interval(self.check_interval);
        // 一轮拖得比检查间隔还长时不补跑错过的那几次：醒来只是看一眼是否到期
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => self.run_if_due().await,
                // 发送端全部关掉也当作停止信号
                _ = shutdown_rx.recv() => {
                    info!("[log_cleanup] shutdown");
                    break;
                }
            }
        }
    }

    /// 到期就清理一轮
    pub(in crate::cleanup_task) async fn run_if_due(&mut self) {
        if !self.due() {
            debug!(
                "[log_cleanup] Not due yet (every {:?})",
                self.cleanup_interval
            );
            return;
        }
        match self.log_cleaner.cleanup_once().await {
            Ok(log_stats) => {
                // 要重试却什么都没删掉，才算停在原地；删掉了东西（哪怕这一轮没做完）是进展
                let stalled = log_stats.needs_retry()
                    && log_stats.files_deleted == 0
                    && log_stats.dirs_deleted == 0;
                self.stalled_sweeps = if stalled {
                    self.stalled_sweeps.saturating_add(1)
                } else {
                    0
                };
                self.last_cleanup = if log_stats.needs_retry() {
                    None
                } else {
                    Some(Instant::now())
                };
                // 无法确认或删除失败说明环境有问题，必须告警；"仍被挂载所以保留"是长期运行
                // 容器的常态，"顺延"是本轮删除预算用完、下一轮继续——这两类只记 debug。
                if log_stats.has_errors() {
                    warn!(
                        "[cleaner] Log cleanup needs attention: {}",
                        log_stats.summary()
                    );
                } else if log_stats.files_deleted > 0 || log_stats.dirs_deleted > 0 {
                    info!("[cleaner] Cleanup completed: {}", log_stats.summary());
                } else {
                    debug!("[cleaner] Log cleanup: {}", log_stats.summary());
                }
                if self.stalled_sweeps >= STALLED_SWEEPS {
                    warn!(
                        sweeps = self.stalled_sweeps,
                        first_deferred = log_stats
                            .first_deferred
                            .as_deref()
                            .map(|path| path.display().to_string()),
                        "[cleaner] Log cleanup keeps retrying without deleting anything: {}",
                        log_stats.summary()
                    );
                }
            }
            // 日志目录卡在扫描里：放弃这一轮，等下一个完整周期再试。底层还没返回的阻塞调用
            // 仍占着线程，不能每次醒来都搭一个进去
            Err(error @ LogCleanupError::ScanTimedOut(timeout)) => {
                self.last_cleanup = Some(Instant::now());
                warn!(
                    timeout_secs = timeout.as_secs(),
                    "[cleaner] Log cleanup scan did not finish within {}s; abandoning this run and retrying at the next log cleanup interval: {}",
                    timeout.as_secs(),
                    error
                );
            }
            // 出错的那一轮自己告警，不动停滞计数
            Err(LogCleanupError::Io(e)) => {
                self.last_cleanup = None;
                warn!("[cleaner] Cleanup failed: {}", e);
            }
        }
    }

    /// 是否到期：从未完成过，或距上一次完成已满一个周期
    pub(in crate::cleanup_task) fn due(&self) -> bool {
        match self.last_cleanup {
            None => true,
            Some(last) => Instant::now().saturating_duration_since(last) >= self.cleanup_interval,
        }
    }

    /// 测试用：让日志目录卡住、变慢
    #[cfg(test)]
    pub(in crate::cleanup_task) fn log_cleaner_mut(&mut self) -> &mut LogCleaner {
        &mut self.log_cleaner
    }
}

#[cfg(test)]
mod tests;
