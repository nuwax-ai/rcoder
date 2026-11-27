//! 复查一直超时的候选被隔离，记录会过期。

use super::*;

/// 复查一直不回来的候选：给了充足的时间仍然超时，连续 3 轮后被隔离——保留目录、
/// 计入隔离数，之后不再问清单、不占预算。
#[tokio::test]
async fn a_candidate_that_keeps_timing_out_is_quarantined_after_three_sweeps() {
    let root = TempDir::new().unwrap();
    let stuck = aged_dir(root.path(), "runner-stuck", 15);
    let inventory = FakeInventory::new(&[]);
    let cleaner = cleaner(root.path(), inventory.clone())
        .with_reclaim_budget(Duration::from_millis(40))
        .with_fair_recheck_window(Duration::from_millis(20))
        .with_recheck_delay(Duration::from_secs(3));

    for _ in 0..3 {
        let stats = cleaner.cleanup_once().await.unwrap();
        assert_eq!(stats.dirs_deferred_by_budget, 1);
        assert_eq!(stats.dirs_quarantined, 0);
        assert_eq!(stats.first_deferred.as_deref(), Some(stuck.as_path()));
    }
    assert_eq!(inventory.calls(), 3);

    // 隔离生效：不问清单、不等复查
    let started = std::time::Instant::now();
    let stats = cleaner.cleanup_once().await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(stats.dirs_quarantined, 1);
    assert_eq!(stats.dirs_deferred_by_budget, 0);
    assert_eq!(inventory.calls(), 3);
    assert!(stats.has_errors());
    assert!(!stats.needs_retry(), "隔离是持续状态，不重置日志清理的周期");
    assert!(stats.summary().contains("quarantined: 1 dirs"));
    assert!(stuck.is_dir());
}

/// 预算本来就快用完时的超时只是没轮到它：不记在候选头上，不会因此被隔离。
#[tokio::test]
async fn a_timeout_caused_by_a_nearly_spent_budget_does_not_count_toward_quarantine() {
    let root = TempDir::new().unwrap();
    let expired = aged_dir(root.path(), "runner-a", 15);
    let inventory = FakeInventory::new(&[]);
    // 默认的公平窗口是 500ms，而整轮预算只有 40ms
    let cleaner = cleaner(root.path(), inventory.clone())
        .with_reclaim_budget(Duration::from_millis(40))
        .with_recheck_delay(Duration::from_secs(3));

    for _ in 0..4 {
        let stats = cleaner.cleanup_once().await.unwrap();
        assert_eq!(stats.dirs_deferred_by_budget, 1);
        assert_eq!(
            stats.dirs_quarantined, 0,
            "no sweep gave this candidate a fair window"
        );
    }
    assert_eq!(inventory.calls(), 4, "it keeps taking part in every sweep");
    assert!(expired.is_dir());
}

/// 复查恢复之后之前的超时记录清零：同名目录之后再慢两轮不会被误隔离。
#[tokio::test]
async fn a_successful_recheck_clears_the_timeout_count() {
    let root = TempDir::new().unwrap();
    let inventory = FakeInventory::new(&[]);
    let cleaner = cleaner(root.path(), inventory.clone())
        .with_reclaim_budget(Duration::from_millis(40))
        .with_fair_recheck_window(Duration::from_millis(20))
        .with_recheck_delay(Duration::from_secs(3));

    let first = aged_dir(root.path(), "runner-a", 15);
    for _ in 0..2 {
        cleaner.cleanup_once().await.unwrap();
    }
    cleaner.set_recheck_delay(Duration::ZERO);
    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(
        stats.dirs_deleted, 1,
        "the recovered candidate is reclaimed"
    );
    assert!(!first.exists());

    let again = aged_dir(root.path(), "runner-a", 15);
    cleaner.set_recheck_delay(Duration::from_secs(3));
    for _ in 0..2 {
        let stats = cleaner.cleanup_once().await.unwrap();
        assert_eq!(stats.dirs_quarantined, 0);
    }
    cleaner.cleanup_once().await.unwrap();
    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(stats.dirs_quarantined, 1);
    assert!(again.is_dir());
}

/// 被隔离的目录不挡路：排在它后面的目录照常回收。
#[tokio::test]
async fn a_quarantined_directory_does_not_block_the_others() {
    let root = TempDir::new().unwrap();
    let inventory = FakeInventory::new(&[]);
    let cleaner = cleaner(root.path(), inventory.clone())
        .with_reclaim_budget(Duration::from_millis(40))
        .with_fair_recheck_window(Duration::from_millis(20))
        .with_recheck_delay(Duration::from_secs(3));
    let stuck = aged_dir(root.path(), "runner-stuck", 15);
    for _ in 0..4 {
        cleaner.cleanup_once().await.unwrap();
    }
    // 文件系统恢复：之后的目录复查正常返回
    cleaner.set_recheck_delay(Duration::ZERO);

    let later = aged_dir(root.path(), "runner-later", 15);
    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.dirs_quarantined, 1);
    assert_eq!(stats.dirs_deferred_by_budget, 0);
    assert!(!later.exists());
    assert!(stuck.is_dir());
}

/// 超时记录会过期，隔离不是永久的：
/// - 两轮之间隔得比保留时长还久，记录失效，计数从头开始，不会被误隔离；
/// - 隔离中的目录过了保留时长会被重新尝试，文件系统恢复了就正常回收。
#[tokio::test]
async fn timeout_records_expire_so_a_quarantined_directory_is_tried_again() {
    let root = TempDir::new().unwrap();
    let expired = aged_dir(root.path(), "runner-a", 15);
    let inventory = FakeInventory::new(&[]);
    let cleaner = cleaner(root.path(), inventory.clone())
        .with_reclaim_budget(Duration::from_millis(10))
        .with_fair_recheck_window(Duration::from_millis(2))
        .with_recheck_delay(Duration::from_secs(3))
        .with_recheck_timeout_memory(Duration::from_secs(1));

    // 第一轮超时之后隔得比保留时长久：记录失效
    cleaner.cleanup_once().await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // 接下来的三轮每轮都超时。记录若没失效，第二轮之后就会隔离
    for _ in 0..3 {
        let stats = cleaner.cleanup_once().await.unwrap();
        assert_eq!(stats.dirs_deferred_by_budget, 1);
        assert_eq!(
            stats.dirs_quarantined, 0,
            "the count started over after the gap"
        );
    }
    // 连续第三轮达到隔离轮数：下一轮起隔离
    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(stats.dirs_quarantined, 1);
    assert_eq!(stats.dirs_deferred_by_budget, 0);

    // 记录过期之后，后面的日志清理再试一次；文件系统已恢复，目录被回收
    cleaner.set_recheck_delay(Duration::ZERO);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(stats.dirs_quarantined, 0);
    assert_eq!(stats.dirs_deleted, 1);
    assert!(!expired.exists());
}

/// 隔离在初次扫描里就生效：已隔离的目录连第一次 metadata 都不读。读它的是和复查同类的
/// 调用，上一次卡在复查的目录，下一轮很可能连扫描都卡住。
#[tokio::test]
async fn a_quarantined_directory_is_skipped_before_its_metadata_is_read() {
    let root = TempDir::new().unwrap();
    let stuck = aged_dir(root.path(), "runner-stuck", 15);
    let inventory = FakeInventory::new(&[]);
    let cleaner = cleaner(root.path(), inventory.clone())
        .with_reclaim_budget(Duration::from_millis(40))
        .with_fair_recheck_window(Duration::from_millis(20))
        .with_recheck_delay(Duration::from_secs(3));
    for _ in 0..3 {
        cleaner.cleanup_once().await.unwrap();
    }

    // 文件系统继续恶化：现在连初次扫描的 metadata 都卡住
    cleaner.set_scan_delay(Duration::from_secs(3));
    let started = std::time::Instant::now();
    let stats = cleaner.cleanup_once().await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "a quarantined directory must not be touched by the scan"
    );
    assert_eq!(stats.dirs_quarantined, 1);
    assert_eq!(inventory.calls(), 3);
    assert!(stuck.is_dir());
}
