//! 挂载判定：候选目录是否还被容器挂着、清单拿不到或无法确认时怎么处理。

use super::*;

/// 单个候选判不出来只保留它自己：其余能判定的目录照常回收。
#[tokio::test]
async fn an_unconfirmable_candidate_is_kept_without_stopping_the_others() {
    let temp_dir = TempDir::new().unwrap();
    let opaque = aged_dir(temp_dir.path(), "runner-opaque-20261029035324", 15);
    let orphan = aged_dir(temp_dir.path(), "runner-gone-20261029035324", 15);
    let inventory = FakeInventory::new(&[]);
    inventory.set_unconfirmed(&opaque);

    let stats = cleaner(temp_dir.path(), inventory)
        .cleanup_once()
        .await
        .unwrap();

    assert!(opaque.exists(), "an unconfirmable directory must be kept");
    assert!(
        !orphan.exists(),
        "a decidable orphan must still be reclaimed"
    );
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.dirs_skipped_unconfirmed, 1);
}

/// 回归：目录按修改时间已过期，但仍有容器把它当 bind mount 源，必须保留。
///
/// 这是线上故障的形态：容器创建时的日志目录被当作过期日志删掉，容器里的
/// `/app/container-logs` 随之失效，依赖它的 agent 启动即超时。
#[tokio::test]
async fn expired_dir_still_mounted_is_kept() {
    let temp_dir = TempDir::new().unwrap();
    let dir = aged_dir(temp_dir.path(), "runner-a-20261029035324", 15);
    let inventory = FakeInventory::new(&[dir.to_str().unwrap()]);

    let stats = cleaner(temp_dir.path(), inventory)
        .cleanup_once()
        .await
        .unwrap();

    assert!(dir.exists(), "mounted log directory must not be deleted");
    assert_eq!(stats.dirs_deleted, 0);
    assert_eq!(stats.dirs_skipped_in_use, 1);
    assert_eq!(stats.dirs_skipped_unconfirmed, 0);
    assert_eq!(stats.failed_deletions, 0);
}

/// 回归（审查意见 P1-1）：容器改名后，按容器名已经查不到它，但挂载还在。
/// 判据必须是"是否被挂载"，否则运行中容器的日志目录会被删掉。
#[tokio::test]
async fn expired_dir_of_renamed_container_is_kept() {
    let temp_dir = TempDir::new().unwrap();
    // 目录名还是创建时的容器名；容器后来被改名，这个名字已经查不到任何容器
    let dir = aged_dir(temp_dir.path(), "runner-original-20261029035324", 15);
    let inventory = FakeInventory::new(&[dir.to_str().unwrap()]);
    // 清单里只有挂载路径，没有任何容器名信息——改名不影响判断
    assert!(
        inventory
            .mounted
            .lock()
            .unwrap()
            .iter()
            .all(|p| !p.ends_with("runner-renamed"))
    );

    let stats = cleaner(temp_dir.path(), inventory)
        .cleanup_once()
        .await
        .unwrap();

    assert!(
        dir.exists(),
        "a renamed container still holds this mount, so the directory must survive"
    );
    assert_eq!(stats.dirs_deleted, 0);
    assert_eq!(stats.dirs_skipped_in_use, 1);
}

/// 已过期且没有任何容器挂它：可以回收。
#[tokio::test]
async fn expired_dir_not_mounted_is_deleted() {
    let temp_dir = TempDir::new().unwrap();
    let dir = aged_dir(temp_dir.path(), "runner-gone-20261029035324", 15);

    let stats = cleaner(temp_dir.path(), FakeInventory::new(&[]))
        .cleanup_once()
        .await
        .unwrap();

    assert!(!dir.exists(), "unmounted log directory should be reclaimed");
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.dirs_skipped_in_use, 0);
    assert_eq!(stats.dirs_skipped_unconfirmed, 0);
}

/// 容器被回收后挂载消失：下一轮目录就能删掉。
#[tokio::test]
async fn dir_is_reclaimed_once_its_mount_is_gone() {
    let temp_dir = TempDir::new().unwrap();
    let dir = aged_dir(temp_dir.path(), "runner-a-20261029035324", 15);
    let inventory = FakeInventory::new(&[dir.to_str().unwrap()]);
    let cleaner = cleaner(temp_dir.path(), inventory.clone());

    let first = cleaner.cleanup_once().await.unwrap();
    assert!(dir.exists());
    assert_eq!(first.dirs_skipped_in_use, 1);

    inventory.set_mounted(dir.to_str().unwrap(), false);
    let second = cleaner.cleanup_once().await.unwrap();
    assert!(!dir.exists());
    assert_eq!(second.dirs_deleted, 1);
}

/// 拿不到挂载清单：本轮一个目录都不删，且只问一次。
#[tokio::test]
async fn missing_inventory_keeps_every_directory_and_probes_once() {
    let temp_dir = TempDir::new().unwrap();
    let a = aged_dir(temp_dir.path(), "runner-a-20261029035324", 15);
    let b = aged_dir(temp_dir.path(), "runner-b-20261029035324", 15);
    let inventory = FakeInventory::new(&[]);
    inventory.set_failing(true);

    let stats = cleaner(temp_dir.path(), inventory.clone())
        .cleanup_once()
        .await
        .unwrap();

    assert!(
        a.exists() && b.exists(),
        "without an inventory nothing may be deleted"
    );
    assert_eq!(stats.dirs_deleted, 0);
    assert_eq!(stats.dirs_skipped_unconfirmed, 2);
    assert_eq!(inventory.calls(), 1);
}

/// 挂载源无法确认指向哪里是持续状态：只计数、保留目录，但不请求"下一轮重试"——重试不会
/// 让它变得可确认，周期照常推进。
#[tokio::test]
async fn a_standing_unconfirmed_candidate_does_not_request_a_retry() {
    let temp_dir = TempDir::new().unwrap();
    let opaque = aged_dir(temp_dir.path(), "runner-opaque-20261029035324", 15);
    let inventory = FakeInventory::new(&[]);
    inventory.set_unconfirmed(&opaque);

    let stats = cleaner(temp_dir.path(), inventory)
        .cleanup_once()
        .await
        .unwrap();

    assert!(opaque.exists());
    assert_eq!(stats.dirs_skipped_unconfirmed, 1);
    assert_eq!(stats.inventory_unavailable, 0);
    assert!(stats.has_errors(), "it is still worth a warning");
    assert!(
        !stats.needs_retry(),
        "a standing unconfirmable source must not reset the schedule"
    );
}

/// 清单拿不到是短暂失败：保留全部候选，并请求下一轮重试。
#[tokio::test]
async fn an_unavailable_inventory_requests_a_retry() {
    let temp_dir = TempDir::new().unwrap();
    aged_dir(temp_dir.path(), "runner-a-20261029035324", 15);
    aged_dir(temp_dir.path(), "runner-b-20261029035324", 15);
    let inventory = FakeInventory::new(&[]);
    inventory.set_failing(true);

    let stats = cleaner(temp_dir.path(), inventory)
        .cleanup_once()
        .await
        .unwrap();

    assert_eq!(stats.dirs_skipped_unconfirmed, 2);
    assert_eq!(stats.inventory_unavailable, 2);
    assert!(stats.needs_retry());
}

/// 每轮最多向运行要一次清单，无论有多少个过期目录。
#[tokio::test]
async fn one_inventory_request_per_cycle() {
    let temp_dir = TempDir::new().unwrap();
    for index in 0..5 {
        aged_dir(
            temp_dir.path(),
            &format!("runner-{index}-20261029035324"),
            15,
        );
    }

    let inventory = FakeInventory::new(&[]);
    let stats = cleaner(temp_dir.path(), inventory.clone())
        .cleanup_once()
        .await
        .unwrap();

    assert_eq!(stats.dirs_deleted, 5);
    assert_eq!(
        inventory.calls(),
        1,
        "five expired directories must cost one inventory request, not one each"
    );
}

/// 没有过期目录时一次接口都不调。
#[tokio::test]
async fn no_inventory_request_when_nothing_is_expired() {
    let temp_dir = TempDir::new().unwrap();
    let fresh = aged_dir(temp_dir.path(), "runner-new-20261125080000", 1);
    let inventory = FakeInventory::new(&[]);

    let stats = cleaner(temp_dir.path(), inventory.clone())
        .cleanup_once()
        .await
        .unwrap();

    assert!(
        fresh.exists(),
        "directory inside the retention window must be kept"
    );
    assert_eq!(stats.dirs_deleted, 0);
    assert_eq!(inventory.calls(), 0);
}

/// 名字不符合布局、也没有容器挂它的过期目录：照常回收。
///
/// 判据是"是否被挂载"而不是"名字是否我们创建的"，所以这类目录不会被永久留下。
#[tokio::test]
async fn expired_dir_with_foreign_name_is_reclaimed_when_not_mounted() {
    let temp_dir = TempDir::new().unwrap();
    let dir = aged_dir(temp_dir.path(), "manual-backup", 15);

    let stats = cleaner(temp_dir.path(), FakeInventory::new(&[]))
        .cleanup_once()
        .await
        .unwrap();

    assert!(!dir.exists(), "unmounted directory should be reclaimed");
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.dirs_skipped_in_use, 0);
}

/// 同一轮里三种目录并存：挂着的、没挂的、未过期的各若干，结果互不影响。
#[tokio::test]
async fn mixed_directories_are_handled_independently() {
    let temp_dir = TempDir::new().unwrap();
    let live = aged_dir(temp_dir.path(), "runner-live-20261029035324", 15);
    let orphan = aged_dir(temp_dir.path(), "runner-gone-20261029035324", 15);
    let fresh = aged_dir(temp_dir.path(), "runner-new-20261125080000", 1);
    let inventory = FakeInventory::new(&[live.to_str().unwrap()]);

    let stats = cleaner(temp_dir.path(), inventory)
        .cleanup_once()
        .await
        .unwrap();

    assert!(live.exists());
    assert!(!orphan.exists());
    assert!(fresh.exists());
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.dirs_skipped_in_use, 1);
    assert_eq!(
        stats.summary(),
        "deleted: 1 dirs, kept: 1 dirs still mounted"
    );
}

/// 一轮清理进行到一半时有容器刚启动、建了自己的日志目录：这一轮的候选在扫描时就定下来了，
/// 碰不到它。清理器因此不必和挂载准备互斥。
#[tokio::test]
async fn a_log_directory_created_while_a_sweep_runs_is_not_touched() {
    let root = TempDir::new().unwrap();
    let expired = aged_dir(root.path(), "runner-expired", 15);
    let inventory = FakeInventory::new(&[]);
    let fresh = root.path().join("runner-fresh-20261126090000");
    inventory.create_on_probe(&fresh);

    let stats = cleaner(root.path(), inventory.clone())
        .cleanup_once()
        .await
        .unwrap();

    assert!(!expired.exists());
    assert!(
        fresh.is_dir(),
        "a directory created after the scan is not a candidate of this sweep"
    );
    assert_eq!(stats.dirs_deleted, 1);

    // 下一轮它也不在保留期内
    cleaner(root.path(), inventory)
        .cleanup_once()
        .await
        .unwrap();
    assert!(fresh.is_dir());
}

/// 回归（Codex 第四轮 P1）：创建方在清理器取完清单**之后**才登记了一个已过期的旧目录。清单里
/// 看不到它，但删除之前的最后一次确认看得到：目录保留，数据不丢。
///
/// 这个顺序是真实可能的：清理器读登记 → 创建方登记旧目录、删旧容器 → Docker 返回清单 → 新容器
/// 还没建出来。
#[tokio::test]
async fn a_directory_registered_after_the_inventory_is_still_kept() {
    let temp_dir = TempDir::new().unwrap();
    // 先放数据再置过期时间：顺序反了的话，写文件会把目录的修改时间刷成现在
    let reused = aged_dir(temp_dir.path(), "runner-a-20261029035324", 15);
    let orphan = aged_dir(temp_dir.path(), "runner-gone-20261029035324", 15);
    std::fs::write(reused.join("keep.log"), b"still bound").unwrap();
    let old_time = std::time::SystemTime::now() - Duration::from_secs(15 * 24 * 60 * 60);
    std::fs::set_times(
        &reused,
        std::fs::FileTimes::new().set_modified(old_time),
    )
    .unwrap();
    let inventory = FakeInventory::new(&[]);
    // 清单读不到它：创建方登记在清理器取完清单之后
    inventory.refuse_reclaim(&reused);

    let stats = cleaner(temp_dir.path(), inventory.clone())
        .cleanup_once()
        .await
        .unwrap();

    assert!(
        reused.exists(),
        "a directory a starting container registered after the inventory must be kept"
    );
    assert!(
        !orphan.exists(),
        "a decidable orphan is still reclaimed"
    );
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.dirs_skipped_in_use, 1);
    assert!(
        reused.join("keep.log").is_file(),
        "what the directory holds survives"
    );
}

/// 最后一次确认判不出来：目录保留，和清单阶段的"无法确认"一样处理
#[tokio::test]
async fn a_directory_the_runtime_cannot_confirm_before_deleting_is_kept() {
    let temp_dir = TempDir::new().unwrap();
    let kept = aged_dir(temp_dir.path(), "runner-a-20261029035324", 15);
    let orphan = aged_dir(temp_dir.path(), "runner-gone-20261029035324", 15);
    let inventory = FakeInventory::new(&[]);
    inventory.refuse_reclaim_as_unconfirmed(&kept);

    let stats = cleaner(temp_dir.path(), inventory)
        .cleanup_once()
        .await
        .unwrap();

    assert!(kept.exists());
    assert!(!orphan.exists());
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.dirs_skipped_unconfirmed, 1);
}

/// 删除之前每个候选都要过一次最后一次确认，被拒的那个不再发起删除
#[tokio::test]
async fn the_final_check_is_asked_for_every_candidate_that_is_about_to_be_deleted() {
    let temp_dir = TempDir::new().unwrap();
    let kept = aged_dir(temp_dir.path(), "runner-a-20261029035324", 15);
    let orphan = aged_dir(temp_dir.path(), "runner-gone-20261029035324", 15);
    let inventory = FakeInventory::new(&[]);
    inventory.refuse_reclaim(&kept);

    let stats = cleaner(temp_dir.path(), inventory.clone())
        .cleanup_once()
        .await
        .unwrap();

    assert_eq!(
        inventory.reclaim_checks(),
        vec![kept.clone(), orphan.clone()],
        "both candidates are confirmed right before deletion, in scan order"
    );
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.dirs_skipped_in_use, 1);
}
