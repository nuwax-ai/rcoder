//! 初次扫描里卡住的条目：读不回来就记一轮、跳过，其余照常回收；连续 3 轮之后隔离，记录过期
//! 之后再试。删除不在限时扫描里：慢删除照旧等到结束，扫描超时也已经确认的那一批照常回收。

use super::*;
use crate::cleanup_task::logs::LogCleanupError;

/// 建一个文件并把它标记为 `days` 天前修改
fn aged_file(parent: &Path, name: &str, days: u64) -> PathBuf {
    let file = parent.join(name);
    std::fs::write(&file, b"old").unwrap();
    let old_time = std::time::SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
    std::fs::set_times(&file, std::fs::FileTimes::new().set_modified(old_time)).unwrap();
    file
}

/// 一个限得出结果的时间：条目时限的三分之一
fn well_within(entry_timeout: Duration) -> Duration {
    entry_timeout / 3
}

/// 卡住的那个条目每次都读不回来：记一轮、跳过，排在后面的目录照常回收。只靠扫描的总时限
/// 的话整次扫描都会作废，什么都回收不了。
#[tokio::test]
async fn an_entry_stuck_in_the_scan_is_skipped_and_the_others_are_still_reclaimed() {
    let root = TempDir::new().unwrap();
    let stuck = aged_dir(root.path(), "runner-stuck", 15);
    let later = aged_dir(root.path(), "runner-later", 15);
    let inventory = FakeInventory::new(&[]);
    let mut cleaner = cleaner(root.path(), inventory.clone());
    cleaner.set_scan_entry_timeout(Duration::from_millis(300));
    cleaner.stall_scan_of(&stuck);

    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(
        stats.dirs_deleted, 1,
        "the other expired directory is reclaimed even though one entry hangs"
    );
    assert!(!later.exists());
    assert_eq!(stats.entries_stuck_in_scan, 1);
    assert_eq!(stats.dirs_quarantined, 0);
    assert_eq!(stats.first_deferred.as_deref(), Some(stuck.as_path()));
    assert!(stats.has_errors());
    assert!(
        stats.needs_retry(),
        "a stuck entry is retried at the next sweep, not a day later"
    );
    assert!(stats.summary().contains("stuck in scan: 1 entries"));
    for _ in 0..2 {
        let stats = cleaner.cleanup_once().await.unwrap();
        assert_eq!(stats.entries_stuck_in_scan, 1);
        assert_eq!(stats.dirs_quarantined, 0);
    }

    // 连续第三轮达到隔离轮数：之后连它的 metadata 都不读
    let started = std::time::Instant::now();
    let stats = cleaner.cleanup_once().await.unwrap();
    assert!(
        started.elapsed() < well_within(Duration::from_millis(300)),
        "a quarantined entry must not be touched by the scan"
    );
    assert_eq!(stats.dirs_quarantined, 1);
    assert_eq!(stats.entries_stuck_in_scan, 0);
    assert!(
        !stats.needs_retry(),
        "quarantine is a lasting state and does not reset the log cleanup period"
    );
    assert!(stuck.is_dir());
}

/// 文件系统恢复、隔离记录过期：条目被重新扫描，正常回收。
#[tokio::test]
async fn a_stuck_entry_is_scanned_again_once_its_record_expires() {
    let root = TempDir::new().unwrap();
    let stuck = aged_dir(root.path(), "runner-stuck", 15);
    let inventory = FakeInventory::new(&[]);
    let mut cleaner = cleaner(root.path(), inventory.clone())
        .with_recheck_timeout_memory(Duration::from_millis(200));
    cleaner.set_scan_entry_timeout(Duration::from_millis(100));
    cleaner.stall_scan_of(&stuck);
    for _ in 0..3 {
        cleaner.cleanup_once().await.unwrap();
    }

    cleaner.unstall_scan();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let stats = cleaner.cleanup_once().await.unwrap();

    assert_eq!(stats.dirs_quarantined, 0);
    assert_eq!(stats.dirs_deleted, 1);
    assert!(!stuck.exists());
}

/// 卡住之后恢复了，之前的超时记录清零：同名条目之后再慢两轮不会被误隔离。
#[tokio::test]
async fn a_successful_read_clears_the_timeout_count() {
    let root = TempDir::new().unwrap();
    let inventory = FakeInventory::new(&[]);
    let mut cleaner = cleaner(root.path(), inventory.clone());
    cleaner.set_scan_entry_timeout(Duration::from_millis(50));

    let first = aged_dir(root.path(), "runner-a", 15);
    cleaner.stall_scan_of(&first);
    for _ in 0..2 {
        let stats = cleaner.cleanup_once().await.unwrap();
        assert_eq!(stats.entries_stuck_in_scan, 1);
    }
    cleaner.unstall_scan();
    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(stats.dirs_deleted, 1, "the recovered entry is reclaimed");
    assert!(!first.exists());

    let again = aged_dir(root.path(), "runner-a", 15);
    cleaner.stall_scan_of(&again);
    for _ in 0..2 {
        let stats = cleaner.cleanup_once().await.unwrap();
        assert_eq!(stats.dirs_quarantined, 0, "the count started over");
    }
    cleaner.cleanup_once().await.unwrap();
    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(stats.dirs_quarantined, 1);
    assert!(again.is_dir());
}

/// 删一个过期文件很慢：删除不在限时扫描里，照旧等到它返回，这一轮照样算完成。
///
/// 回归（Codex 第四轮 P2）：以前文件删除被包在扫描的逐项时限里。等待超时之后底层的 unlink 还在
/// 队列上，可能删掉同一路径上后来建的东西；现在删除不设时限，也就不再放弃。
#[tokio::test]
async fn a_slow_file_removal_does_not_abort_the_sweep() {
    let root = TempDir::new().unwrap();
    let slow = aged_file(root.path(), "old-slow.log", 15);
    let other = aged_file(root.path(), "old-other.log", 15);
    let inventory = FakeInventory::new(&[]);
    let mut cleaner = cleaner(root.path(), inventory);
    // 删除比整轮扫描的时限还慢：换成以前的做法，这一轮会以 ScanTimedOut 收场
    cleaner.set_scan_timeout(Duration::from_millis(150));
    cleaner.set_removal_delay(Duration::from_millis(400));

    let stats = cleaner.cleanup_once().await.unwrap();

    assert_eq!(stats.files_deleted, 2, "both files are deleted");
    assert!(!slow.exists());
    assert!(!other.exists());
    assert_eq!(stats.failed_deletions, 0);
}

/// 扫描超时：超时之前已经确认的那一批照常回收，然后这一轮按"没扫完"报告。
///
/// 回归（Codex 第四轮 P2）：以前扫描超时会丢掉整轮，排在卡住条目后面的健康目录什么都删不掉。
#[tokio::test]
async fn a_timed_out_scan_still_reclaims_what_it_confirmed() {
    let root = TempDir::new().unwrap();
    for name in ["runner-a", "runner-b", "runner-c"] {
        aged_dir(root.path(), &format!("{name}-20261029035324"), 15);
    }
    let inventory = FakeInventory::new(&[]);
    let mut cleaner = cleaner(root.path(), inventory);
    // 每个条目读 20 毫秒，整轮 50 毫秒：前两个读完了，第三个读到一半被取消
    cleaner.set_scan_timeout(Duration::from_millis(50));
    cleaner.set_scan_entry_timeout(Duration::from_secs(30));
    cleaner.set_scan_delay(Duration::from_millis(20));

    let error = cleaner
        .cleanup_once()
        .await
        .expect_err("the scan did not finish");

    assert!(
        matches!(error, LogCleanupError::ScanTimedOut(_)),
        "the sweep still reports that the scan did not finish: {error}"
    );
    assert_eq!(
        std::fs::read_dir(root.path()).unwrap().count(),
        1,
        "the entry that was never scanned is left alone, the confirmed ones are reclaimed"
    );
}
