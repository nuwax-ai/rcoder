//! 日志清理器的单元测试：用临时目录和假的挂载判定，不依赖 Docker / K8s。
//!
//! 按主题分在几个文件里：预算与批次、挂载判定、保留期、卡住候选的隔离、扫描里卡住的条目、
//! 统计摘要；这里是最基本的几个。夹具在 [`super::testing`]。

mod budget;
mod mounts;
mod quarantine;
mod retention;
mod scan;
mod stats;

use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::TempDir;
use tokio::fs;

use super::cleaner::LogCleaner;
use super::stats::LogCleanupStats;
use super::testing::{FakeInventory, aged_dir, cleaner};

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
