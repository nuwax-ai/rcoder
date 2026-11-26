//! 日志清理器的单元测试：用临时目录和假的挂载判定，不依赖 Docker / K8s。
//!
//! 按主题分在几个文件里：预算与批次、挂载判定、保留期、卡住候选的隔离、统计摘要。

use async_trait::async_trait;
use container_runtime_api::LogDirectoryMounts;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::fs;

use super::cleaner::LogCleaner;
use super::owner::MountedPathsProbe;
use super::stats::LogCleanupStats;

/// 假的挂载判定：`mounted` 里的路径回答"仍被容器挂着"，`unconfirmed` 里的回答"无法确认"，
/// 其余路径回答"没有容器挂"；`failing` 为 true 时整份清单拿不到。同时记录被询问的次数。
pub(super) struct FakeInventory {
    /// 被认为"仍被挂载"的宿主机路径；测试中途可以改动，用来模拟容器被回收
    pub(super) mounted: Mutex<HashSet<PathBuf>>,
    /// 被判为"无法确认"的路径
    pub(super) unconfirmed: Mutex<Vec<PathBuf>>,
    /// 被询问时顺手删掉的目录，用来模拟"列目录之后、删除之前目录消失"
    pub(super) vanish_on_probe: Mutex<Vec<PathBuf>>,
    /// 被询问时顺手建出来的目录，用来模拟"扫描之后有容器刚启动"
    pub(super) create_on_probe: Mutex<Vec<PathBuf>>,
    pub(super) failing: Mutex<bool>,
    /// 回答之前先等这么久，用来模拟很慢的 Docker
    pub(super) delay: Mutex<Duration>,
    pub(super) calls: AtomicUsize,
}

impl FakeInventory {
    pub(super) fn new(mounted: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            mounted: Mutex::new(mounted.iter().map(PathBuf::from).collect()),
            unconfirmed: Mutex::new(Vec::new()),
            vanish_on_probe: Mutex::new(Vec::new()),
            create_on_probe: Mutex::new(Vec::new()),
            failing: Mutex::new(false),
            delay: Mutex::new(Duration::ZERO),
            calls: AtomicUsize::new(0),
        })
    }

    pub(super) fn set_mounted(&self, path: &str, mounted: bool) {
        let mut set = self.mounted.lock().unwrap();
        if mounted {
            set.insert(PathBuf::from(path));
        } else {
            set.remove(PathBuf::from(path).as_path());
        }
    }

    pub(super) fn set_unconfirmed(&self, path: &Path) {
        self.unconfirmed.lock().unwrap().push(path.to_path_buf());
    }

    pub(super) fn vanish_on_probe(&self, path: &Path) {
        self.vanish_on_probe
            .lock()
            .unwrap()
            .push(path.to_path_buf());
    }

    pub(super) fn create_on_probe(&self, path: &Path) {
        self.create_on_probe
            .lock()
            .unwrap()
            .push(path.to_path_buf());
    }

    pub(super) fn set_failing(&self, failing: bool) {
        *self.failing.lock().unwrap() = failing;
    }

    pub(super) fn set_delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    pub(super) fn calls(&self) -> usize {
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
        for path in self.create_on_probe.lock().unwrap().drain(..) {
            std::fs::create_dir_all(&path).expect("create the directory of a starting container");
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
pub(super) fn aged_dir(parent: &Path, name: &str, days: u64) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).expect("create dir");
    let old_time = std::time::SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
    std::fs::set_times(&dir, std::fs::FileTimes::new().set_modified(old_time))
        .expect("set directory mtime");
    dir
}

pub(super) fn cleaner(log_dir: &Path, inventory: Arc<FakeInventory>) -> LogCleaner {
    LogCleaner::new(log_dir.to_str().expect("utf-8 path"), 10, inventory)
}
mod budget;
mod mounts;
mod quarantine;
mod retention;
mod stats;

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
