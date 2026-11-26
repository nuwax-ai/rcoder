//! 一轮回收的预算：截止时间、每轮的删除批次上限，预算用完后余下的留到下一轮。

use super::*;

/// 一轮里删除目录的时间有预算，从开始判定起计：到期后不再发起删除，余下的留到下一轮。
/// 预算在复查每个候选之前和真正发起删除之前各查一次，复查目录所花的时间也算在预算里。
#[tokio::test]
async fn deletion_budget_defers_the_rest_until_the_next_cycle() {
    let temp_dir = TempDir::new().unwrap();
    let first = aged_dir(temp_dir.path(), "runner-a-20261029035324", 15);
    let second = aged_dir(temp_dir.path(), "runner-b-20261029035324", 15);
    let inventory = FakeInventory::new(&[]);

    // 零预算：任何一次删除发起之前预算都已经用完
    let exhausted = cleaner(temp_dir.path(), inventory.clone()).with_reclaim_budget(Duration::ZERO);
    let stats = exhausted.cleanup_once().await.unwrap();

    assert!(first.exists() && second.exists());
    assert_eq!(stats.dirs_deleted, 0);
    assert_eq!(stats.dirs_deferred_by_budget, 2);
    assert_eq!(stats.dirs_skipped_unconfirmed, 0);
    // 下一轮预算恢复，两个目录都被回收
    assert_eq!(
        cleaner(temp_dir.path(), inventory)
            .cleanup_once()
            .await
            .unwrap()
            .dirs_deleted,
        2
    );
    assert!(!first.exists() && !second.exists());
}

/// 取清单、逐个判定和删除共用同一个截止时间：判定迟迟不回来时，到期就保留全部候选，而不是
/// 一直等下去。
#[tokio::test]
async fn a_slow_observation_keeps_every_directory_until_the_budget_is_spent() {
    let temp_dir = TempDir::new().unwrap();
    let first = aged_dir(temp_dir.path(), "runner-a-20261029035324", 15);
    let second = aged_dir(temp_dir.path(), "runner-b-20261029035324", 15);
    let inventory = FakeInventory::new(&[]);
    inventory.set_delay(Duration::from_secs(3));

    let started = std::time::Instant::now();
    let stats = cleaner(temp_dir.path(), inventory.clone())
        .with_reclaim_budget(Duration::from_millis(50))
        .cleanup_once()
        .await
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "an unfinished observation must not hold up the sweep: {:?}",
        started.elapsed()
    );
    assert!(first.exists() && second.exists());
    assert_eq!(stats.dirs_skipped_unconfirmed, 2);
    assert_eq!(stats.dirs_deleted, 0);
    assert_eq!(
        stats.dirs_deferred_by_budget, 0,
        "an unfinished observation is reported as unconfirmed, not as a spent deletion budget"
    );

    inventory.set_delay(Duration::ZERO);
    let stats = cleaner(temp_dir.path(), inventory)
        .cleanup_once()
        .await
        .unwrap();
    assert!(!first.exists() && !second.exists());
    assert_eq!(stats.dirs_deleted, 2);
}

#[tokio::test]
async fn a_slow_last_metadata_read_is_deferred_within_the_same_budget() {
    let root = TempDir::new().unwrap();
    let expired = aged_dir(root.path(), "runner-a", 15);
    let inventory = FakeInventory::new(&[]);
    let cleaner = cleaner(root.path(), inventory.clone())
        .with_reclaim_budget(Duration::from_millis(100))
        .with_recheck_delay(Duration::from_secs(3));

    let started = std::time::Instant::now();
    let stats = cleaner.cleanup_once().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(inventory.calls(), 1);
    assert_eq!(stats.dirs_deferred_by_budget, 1);
    assert_eq!(stats.dirs_skipped_unconfirmed, 0);
    assert!(expired.is_dir());
}

/// 每轮最多发起一批目录删除，余下的留到下一轮。时间预算管不住"每个都很快、合起来却很多"
/// 的批次，一轮的工作量还得有数量上限。
#[tokio::test]
async fn each_cycle_reclaims_at_most_one_batch_of_directories() {
    let temp_dir = TempDir::new().unwrap();
    let dirs: Vec<PathBuf> = (0..3)
        .map(|index| {
            aged_dir(
                temp_dir.path(),
                &format!("runner-{index}-20261029035324"),
                15,
            )
        })
        .collect();
    let inventory = FakeInventory::new(&[]);

    let limited = cleaner(temp_dir.path(), inventory.clone()).with_reclaim_batch(2);
    let stats = limited.cleanup_once().await.unwrap();

    assert_eq!(stats.dirs_deleted, 2);
    assert_eq!(stats.dirs_deferred_by_budget, 1, "the third must wait");
    assert_eq!(dirs.iter().filter(|dir| dir.exists()).count(), 1);

    // 下一轮接着删
    let stats = cleaner(temp_dir.path(), inventory)
        .cleanup_once()
        .await
        .unwrap();
    assert_eq!(stats.dirs_deleted, 1);
    assert!(dirs.iter().all(|dir| !dir.exists()));
}

/// 失败的删除同样占用这一轮的名额：删除一直失败的积压也只发起一批，不能把整份清单都试一遍。
#[tokio::test]
async fn failed_deletions_consume_the_batch() {
    let temp_dir = TempDir::new().unwrap();
    let dirs: Vec<PathBuf> = (0..3)
        .map(|index| {
            aged_dir(
                temp_dir.path(),
                &format!("runner-{index}-20261029035324"),
                15,
            )
        })
        .collect();
    let inventory = FakeInventory::new(&[]);

    let stats = cleaner(temp_dir.path(), inventory)
        .with_reclaim_batch(2)
        .failing_deletions()
        .cleanup_once()
        .await
        .unwrap();

    assert_eq!(stats.failed_deletions, 2, "only one batch may be started");
    assert_eq!(stats.dirs_deferred_by_budget, 1, "the third must wait");
    assert!(
        dirs.iter().all(|dir| dir.exists()),
        "the fixture must not delete anything"
    );
}

/// 时间预算用完之后不再对余下的候选做任何 I/O。列目录之后目录已经消失时，这一点看得出来：
/// 预算没用完的话会去复查它、发现它已经不在；预算已用完则直接按"预算用完而顺延"计数。
#[tokio::test]
async fn nothing_is_rechecked_once_the_budget_is_spent() {
    let temp_dir = TempDir::new().unwrap();
    let vanished = aged_dir(temp_dir.path(), "runner-vanished-20261029035324", 15);
    let inventory = FakeInventory::new(&[]);
    inventory.vanish_on_probe(&vanished);

    let stats = cleaner(temp_dir.path(), inventory)
        .with_reclaim_budget(Duration::ZERO)
        .cleanup_once()
        .await
        .unwrap();

    assert!(!vanished.exists());
    assert_eq!(
        stats.dirs_deferred_by_budget, 1,
        "the budget is checked before a directory is re-checked"
    );
    assert_eq!(stats.dirs_skipped_unconfirmed, 0);
    assert_eq!(stats.failed_deletions, 0);
}

/// 列目录之后、删除之前目录已经消失：既不算删除成功，也不告警。
#[tokio::test]
async fn a_directory_that_vanishes_before_deletion_is_not_reported_as_a_failure() {
    let temp_dir = TempDir::new().unwrap();
    let vanished = aged_dir(temp_dir.path(), "runner-vanished-20261029035324", 15);
    let inventory = FakeInventory::new(&[]);
    inventory.vanish_on_probe(&vanished);

    let stats = cleaner(temp_dir.path(), inventory)
        .cleanup_once()
        .await
        .unwrap();

    assert!(!vanished.exists());
    assert_eq!(stats.dirs_deleted, 0);
    assert_eq!(stats.dirs_skipped_unconfirmed, 0);
    assert_eq!(stats.failed_deletions, 0);
}
