//! 一轮日志清理的统计摘要。

use super::*;

#[test]
fn stats_summary_reports_kept_deferred_and_failed_items() {
    let stats = LogCleanupStats {
        files_deleted: 2,
        bytes_freed: 3 * 1024 * 1024,
        dirs_skipped_in_use: 1,
        dirs_skipped_unconfirmed: 1,
        dirs_deferred_by_budget: 5,
        failed_deletions: 1,
        ..LogCleanupStats::default()
    };
    assert_eq!(
        stats.summary(),
        "Deleted: 2 files, freed: 3.00 MB, kept: 1 dirs still mounted, \
         kept: 1 dirs without a confirmed mount status, \
         deferred: 5 dirs (reclaim budget spent), failed: 1"
    );
    assert_eq!(LogCleanupStats::default().summary(), "No expired logs");
}
