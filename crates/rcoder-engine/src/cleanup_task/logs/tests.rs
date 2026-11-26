//! 日志清理器的单元测试：用临时目录和假的挂载判定，不依赖 Docker / K8s。

use async_trait::async_trait;
use container_runtime_api::LogDirectoryMounts;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::fs;

use super::cleaner::{LogCleaner, LogCleanupStats};
use super::owner::MountedPathsProbe;

/// 假的挂载判定：`mounted` 里的路径回答"仍被容器挂着"，`unconfirmed` 里的回答"无法确认"，
/// 其余路径回答"没有容器挂"；`failing` 为 true 时整份清单拿不到。同时记录被询问的次数。
struct FakeInventory {
    /// 被认为"仍被挂载"的宿主机路径；测试中途可以改动，用来模拟容器被回收
    mounted: Mutex<HashSet<PathBuf>>,
    /// 被判为"无法确认"的路径
    unconfirmed: Mutex<Vec<PathBuf>>,
    /// 被询问时顺手删掉的目录，用来模拟"列目录之后、删除之前目录消失"
    vanish_on_probe: Mutex<Vec<PathBuf>>,
    failing: Mutex<bool>,
    /// 回答之前先等这么久，用来模拟很慢的 Docker
    delay: Mutex<Duration>,
    calls: AtomicUsize,
}

impl FakeInventory {
    fn new(mounted: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            mounted: Mutex::new(mounted.iter().map(PathBuf::from).collect()),
            unconfirmed: Mutex::new(Vec::new()),
            vanish_on_probe: Mutex::new(Vec::new()),
            failing: Mutex::new(false),
            delay: Mutex::new(Duration::ZERO),
            calls: AtomicUsize::new(0),
        })
    }

    fn set_mounted(&self, path: &str, mounted: bool) {
        let mut set = self.mounted.lock().unwrap();
        if mounted {
            set.insert(PathBuf::from(path));
        } else {
            set.remove(PathBuf::from(path).as_path());
        }
    }

    fn set_unconfirmed(&self, path: &Path) {
        self.unconfirmed.lock().unwrap().push(path.to_path_buf());
    }

    fn vanish_on_probe(&self, path: &Path) {
        self.vanish_on_probe
            .lock()
            .unwrap()
            .push(path.to_path_buf());
    }

    fn set_failing(&self, failing: bool) {
        *self.failing.lock().unwrap() = failing;
    }

    fn set_delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl MountedPathsProbe for FakeInventory {
    async fn mounted_log_directories(
        &self,
        _candidates: &[PathBuf],
    ) -> Result<LogDirectoryMounts, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if *self.failing.lock().unwrap() {
            return Err("docker unreachable".to_string());
        }
        let delay = *self.delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        for path in self.vanish_on_probe.lock().unwrap().drain(..) {
            std::fs::remove_dir_all(path).expect("remove the vanished directory");
        }
        Ok(LogDirectoryMounts {
            mounted: self.mounted.lock().unwrap().clone(),
            unconfirmed: self
                .unconfirmed
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .map(|path| (path, "fixture cannot confirm it".to_owned()))
                .collect(),
        })
    }
}

/// 建一个内容为空的目录并把它标记为 `days` 天前修改
fn aged_dir(parent: &Path, name: &str, days: u64) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).expect("create dir");
    let old_time = std::time::SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
    std::fs::set_times(&dir, std::fs::FileTimes::new().set_modified(old_time))
        .expect("set directory mtime");
    dir
}

fn cleaner(log_dir: &Path, inventory: Arc<FakeInventory>) -> LogCleaner {
    LogCleaner::new(log_dir.to_str().expect("utf-8 path"), 10, inventory)
}

/// 挂载准备正在进行时取不到锁：本轮顺延、不询问判定，也不算故障。
#[tokio::test]
async fn cleanup_defers_while_mount_preparation_holds_the_gate() {
    let root = TempDir::new().unwrap();
    let expired = aged_dir(root.path(), "retired", 15);
    let inventory = FakeInventory::new(&[]);
    let cleaner = cleaner(root.path(), inventory.clone());
    let gate = docker_manager::try_begin_log_mount_cleanup().unwrap();

    let stats = cleaner.cleanup_once().await.unwrap();

    assert!(expired.is_dir());
    assert_eq!(stats.dirs_deferred_while_mounting, 1);
    assert_eq!(
        stats.dirs_skipped_unconfirmed, 0,
        "waiting for a mount preparation is not a failure"
    );
    assert_eq!(inventory.calls(), 0);
    drop(gate);
    assert_eq!(cleaner.cleanup_once().await.unwrap().dirs_deleted, 1);
}

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
    assert_eq!(stats.dirs_deferred_while_mounting, 0);
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

/// 取清单和逐个判定都在写锁里，和删除共用同一个截止时间：判定迟迟不回来时，到期就保留全部
/// 候选、放锁，而不是一直等下去。
#[tokio::test]
async fn a_slow_observation_keeps_every_directory_and_releases_the_gate() {
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
        "an unfinished observation must not hold the gate: {:?}",
        started.elapsed()
    );
    assert!(first.exists() && second.exists());
    assert_eq!(stats.dirs_skipped_unconfirmed, 2);
    assert_eq!(stats.dirs_deleted, 0);
    assert_eq!(
        stats.dirs_deferred_by_budget, 0,
        "an unfinished observation is reported as unconfirmed, not as a spent deletion budget"
    );
    assert!(
        docker_manager::try_begin_log_mount_cleanup().is_some(),
        "the gate must be released while the observation is unfinished"
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
    assert!(docker_manager::try_begin_log_mount_cleanup().is_some());
}

/// 每轮最多发起一批目录删除，余下的留到下一轮。时间预算管不住"每个都很快、合起来却很多"
/// 的批次，持锁期间的工作量还得有数量上限。
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

#[test]
fn stats_summary_reports_kept_deferred_and_failed_items() {
    let stats = LogCleanupStats {
        files_deleted: 2,
        bytes_freed: 3 * 1024 * 1024,
        dirs_skipped_in_use: 1,
        dirs_skipped_unconfirmed: 1,
        dirs_deferred_while_mounting: 4,
        dirs_deferred_by_budget: 5,
        failed_deletions: 1,
        ..LogCleanupStats::default()
    };
    assert_eq!(
        stats.summary(),
        "Deleted: 2 files, freed: 3.00 MB, kept: 1 dirs still mounted, \
         kept: 1 dirs without a confirmed mount status, \
         deferred: 4 dirs (mount preparation in progress), \
         deferred: 5 dirs (reclaim budget spent), failed: 1"
    );
    assert_eq!(LogCleanupStats::default().summary(), "No expired logs");
}

#[tokio::test]
async fn test_log_cleanup_basic() {
    // 创建临时测试目录
    let temp_dir = TempDir::new().unwrap();
    let log_dir = temp_dir.path().join("logs");

    // 创建日志目录
    fs::create_dir_all(&log_dir).await.unwrap();

    // 创建一些测试日志文件
    let test_files = vec![
        ("old_log_1.txt", "old content 1"),
        ("old_log_2.txt", "old content 2"),
        ("recent_log.txt", "recent content"),
    ];

    for (filename, content) in &test_files {
        let file_path = log_dir.join(filename);
        let mut file = std::fs::File::create(&file_path).unwrap();
        std::io::Write::write_all(&mut file, content.as_bytes()).unwrap();
    }

    // 设置旧文件的修改时间为 15 天前
    let old_time = std::time::SystemTime::now() - Duration::from_secs(15 * 24 * 60 * 60);
    for (filename, _) in &test_files[..2] {
        let file_path = log_dir.join(filename);
        if let Err(e) =
            std::fs::set_times(&file_path, std::fs::FileTimes::new().set_modified(old_time))
        {
            // 系统不支持设置修改时间，跳过此测试
            println!(" set mtime not supported, skip: {}", e);
            return;
        }
    }

    // 创建日志清理器，保留 10 天
    let cleaner = cleaner(&log_dir, FakeInventory::new(&[]));

    // 执行清理
    let stats = cleaner.cleanup_once().await.unwrap();

    // 验证结果
    assert_eq!(stats.files_deleted, 2);
    assert!(log_dir.join("recent_log.txt").exists());
    assert!(!log_dir.join("old_log_1.txt").exists());
    assert!(!log_dir.join("old_log_2.txt").exists());
}

#[tokio::test]
async fn test_cleanup_nonexistent_dir() {
    let cleaner = cleaner(Path::new("/nonexistent/path"), FakeInventory::new(&[]));
    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(stats.files_deleted, 0);
    assert_eq!(stats.failed_deletions, 0);
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

/// 保留天数是不受信任的配置输入，换算成秒时不能回绕。
///
/// `1 << 57` 天（即 144115188075855872）直接乘以一天的秒数正好回绕成 0 秒，也就是保留期
/// 变成 0 天——任何目录都会被判为过期。release 构建下这是静默的，debug 构建下直接 panic。
#[tokio::test]
async fn oversized_retention_does_not_wrap_into_zero() {
    let temp_dir = TempDir::new().unwrap();
    // 一个 11 天前的目录，没有任何容器挂它
    let dir = aged_dir(temp_dir.path(), "runner-gone-20261029035324", 11);
    let days_that_wrap_to_zero: u64 = 1 << 57;
    let cleaner = LogCleaner::new(
        temp_dir.path().to_str().unwrap(),
        days_that_wrap_to_zero,
        FakeInventory::new(&[]),
    );

    let stats = cleaner.cleanup_once().await.unwrap();

    assert!(
        cleaner.retention_duration() > Duration::from_secs(11 * 24 * 60 * 60),
        "保留期必须仍然大于这个目录的年龄"
    );
    assert!(dir.exists(), "回绕成 0 天保留期会删掉一切");
    assert_eq!(stats.dirs_deleted, 0);
}

/// 复查一直不回来的候选：给了充足的时间仍然超时，连续 3 轮后被隔离——保留目录、
/// 计入隔离数，之后不再取锁、不问清单、不占预算。
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

    // 隔离生效：不取锁、不问清单、不等复查
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
    assert!(docker_manager::try_begin_log_mount_cleanup().is_some());
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
/// - 两轮之间隔得比保留时长还久，计数每次从头开始，到不了隔离轮数；
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
        .with_recheck_timeout_memory(Duration::from_millis(300));

    // 每轮之后都隔得比保留时长久：每轮都从 1 记起，到不了隔离轮数
    for _ in 0..3 {
        let stats = cleaner.cleanup_once().await.unwrap();
        assert_eq!(stats.dirs_quarantined, 0);
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    // 上一轮是第三次超时：记录若还在，这一轮就该隔离了
    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(
        stats.dirs_deferred_by_budget, 1,
        "the count started over after a long gap"
    );
    assert_eq!(stats.dirs_quarantined, 0);

    // 连续第 2、3 轮：达到隔离轮数，再下一轮起隔离
    for _ in 0..2 {
        cleaner.cleanup_once().await.unwrap();
    }
    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(stats.dirs_quarantined, 1);
    assert_eq!(stats.dirs_deferred_by_budget, 0);

    // 记录过期之后，后面的日志清理再试一次；文件系统已恢复，目录被回收
    cleaner.set_recheck_delay(Duration::ZERO);
    tokio::time::sleep(Duration::from_millis(400)).await;
    let stats = cleaner.cleanup_once().await.unwrap();
    assert_eq!(stats.dirs_quarantined, 0);
    assert_eq!(stats.dirs_deleted, 1);
    assert!(!expired.exists());
}
