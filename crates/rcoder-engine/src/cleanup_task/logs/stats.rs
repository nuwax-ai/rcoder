//! 一轮日志清理的统计：删了什么、留了什么、哪些要下一轮接着做。

use std::path::{Path, PathBuf};

use super::cleaner::RECHECK_TIMEOUT_QUARANTINE_SWEEPS;

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
    /// 本轮第一个被顺延的目录（预算用完，或复查超时），告警时用来定位
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
    pub(super) fn note_deferred(&mut self, path: Option<&Path>) {
        if self.first_deferred.is_none()
            && let Some(path) = path
        {
            self.first_deferred = Some(path.to_path_buf());
        }
    }

    /// 这一轮有没有"本该做完却没做完"、值得下一轮清扫接着做的事：清单拿不到、复查出错、
    /// 删除失败、条目读取失败、批次或时间预算顺延。
    ///
    /// 不含"挂载源无法确认指向哪里"和"目录在扫描后变了"：前者是持续状态（只要那个容器还在，
    /// 每轮都会得到同样的结论，重试不会变得可确认），后者是正常竞态。它们只计数、照常保留，
    /// 不重置日志清理的周期。
    pub(crate) fn needs_retry(&self) -> bool {
        self.inventory_unavailable > 0
            || self.recheck_errors > 0
            || self.failed_deletions > 0
            || self.entry_inspection_errors > 0
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
