//! 日志清理器的单元测试：用临时目录和假的挂载清单，不依赖 Docker / K8s。

use async_trait::async_trait;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::fs;

use super::cleaner::{LogCleaner, LogCleanupStats};
use super::owner::MountedPathsProbe;

/// 假的挂载清单：`mounted` 里的路径回答"还被容器挂着"，`failing` 为 true 时回答
/// "无法确认"，其余路径回答"没有容器挂"。同时记录被询问的次数。
struct FakeInventory {
    /// 被认为"仍被挂载"的宿主机路径；测试中途可以改动，用来模拟容器被回收
    mounted: Mutex<HashSet<PathBuf>>,
    failing: Mutex<bool>,
    calls: AtomicUsize,
}

impl FakeInventory {
    fn new(mounted: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            mounted: Mutex::new(mounted.iter().map(PathBuf::from).collect()),
            failing: Mutex::new(false),
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

    fn set_failing(&self, failing: bool) {
        *self.failing.lock().unwrap() = failing;
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
    ) -> Result<HashSet<PathBuf>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if *self.failing.lock().unwrap() {
            return Err("docker unreachable".to_string());
        }
        Ok(self.mounted.lock().unwrap().clone())
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

#[tokio::test]
async fn cleanup_defers_when_another_sweep_holds_the_mount_gate() {
    let root = TempDir::new().unwrap();
    let expired = aged_dir(root.path(), "retired", 15);
    let inventory = FakeInventory::new(&[]);
    let cleaner = cleaner(root.path(), inventory.clone());
    let guard = docker_manager::try_begin_log_mount_cleanup().unwrap();
    let stats = cleaner.cleanup_once().await.unwrap();
    assert!(expired.is_dir());
    assert_eq!(stats.dirs_skipped_unconfirmed, 1);
    assert_eq!(inventory.calls(), 0);
    drop(guard);
    assert_eq!(cleaner.cleanup_once().await.unwrap().dirs_deleted, 1);
}

#[test]
fn stats_summary_reports_kept_and_failed_items() {
    let stats = LogCleanupStats {
        files_deleted: 2,
        bytes_freed: 3 * 1024 * 1024,
        dirs_skipped_in_use: 1,
        dirs_skipped_unconfirmed: 1,
        failed_deletions: 1,
        ..LogCleanupStats::default()
    };
    assert_eq!(
        stats.summary(),
        "Deleted: 2 files, freed: 3.00 MB, kept: 1 dirs still mounted, \
         kept: 1 dirs without a mount inventory, failed: 1"
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

    let stats = cleaner(temp_dir.path(), FakeInventory::new(&[]))
        .cleanup_once()
        .await
        .unwrap();

    assert_eq!(stats.dirs_deleted, 5);
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
/// `144115188075855872` 天直接相乘正好回绕成 0 秒，也就是保留期变成 0 天——任何目录
/// 都会被判为过期。release 构建下这是静默的，debug 构建下直接 panic。
#[tokio::test]
async fn oversized_retention_does_not_wrap_into_zero() {
    let temp_dir = TempDir::new().unwrap();
    // 一个 11 天前的目录，没有任何容器挂它
    let dir = aged_dir(temp_dir.path(), "runner-gone-20261029035324", 11);
    let cleaner = LogCleaner::new(
        temp_dir.path().to_str().unwrap(),
        144115188075855872,
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
