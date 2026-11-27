//! 初次扫描里卡住的条目：读不回来（或删不回来）就记一轮、跳过，其余照常回收；连续 3 轮之后
//! 隔离，记录过期之后再试。

use super::*;

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

/// 删一个过期文件一直不回来：同样记一轮、跳过，其余照常处理。
#[tokio::test]
async fn an_expired_file_whose_removal_never_returns_is_skipped() {
    let root = TempDir::new().unwrap();
    let stuck = aged_file(root.path(), "old-stuck.log", 15);
    let other = aged_file(root.path(), "old-other.log", 15);
    let inventory = FakeInventory::new(&[]);
    let mut cleaner = cleaner(root.path(), inventory.clone());
    cleaner.set_scan_entry_timeout(Duration::from_millis(100));
    cleaner.stall_scan_removal_of(&stuck);

    let stats = cleaner.cleanup_once().await.unwrap();

    assert_eq!(stats.files_deleted, 1);
    assert_eq!(stats.failed_deletions, 0);
    assert_eq!(stats.entries_stuck_in_scan, 1);
    assert_eq!(stats.first_deferred.as_deref(), Some(stuck.as_path()));
    assert!(stuck.is_file());
    assert!(!other.exists());
}
