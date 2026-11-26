//! 保留期：换算不回绕，保留下限。

use super::*;

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

/// 保留期配成 0 天时仍有保留下限：几秒前刚建出来的目录、刚写的文件不算过期。"建目录"到
/// "Docker 登记这个挂载"之间扫到它时清单里还没有这个容器，没有下限就会删掉正在使用的目录。
#[tokio::test]
async fn a_zero_retention_still_spares_what_was_just_created() {
    let root = TempDir::new().unwrap();
    let moments_ago = std::time::SystemTime::now() - Duration::from_secs(2);
    let fresh = root.path().join("runner-fresh-20261126090000");
    std::fs::create_dir(&fresh).unwrap();
    std::fs::set_times(&fresh, std::fs::FileTimes::new().set_modified(moments_ago)).unwrap();
    let fresh_file = root.path().join("stray.log");
    std::fs::write(&fresh_file, b"just written").unwrap();
    std::fs::set_times(
        &fresh_file,
        std::fs::FileTimes::new().set_modified(moments_ago),
    )
    .unwrap();
    let gone = aged_dir(root.path(), "runner-gone", 15);
    let stray = root.path().join("old-stray.log");
    std::fs::write(&stray, b"old").unwrap();
    let long_ago = std::time::SystemTime::now() - Duration::from_secs(15 * 24 * 60 * 60);
    std::fs::set_times(&stray, std::fs::FileTimes::new().set_modified(long_ago)).unwrap();
    let inventory = FakeInventory::new(&[]);
    let cleaner = LogCleaner::new(root.path().to_str().unwrap(), 0, inventory.clone());

    let stats = cleaner.cleanup_once().await.unwrap();

    assert!(
        fresh.is_dir(),
        "a directory created moments ago is not expired"
    );
    assert!(fresh_file.is_file(), "nor is a file written moments ago");
    assert!(!gone.exists());
    assert!(!stray.exists());
    assert_eq!(stats.dirs_deleted, 1);
    assert_eq!(stats.files_deleted, 1);

    // 下限过去之后照常回收
    std::fs::set_times(&fresh, std::fs::FileTimes::new().set_modified(long_ago)).unwrap();
    let stats = cleaner.cleanup_once().await.unwrap();
    assert!(!fresh.exists());
    assert_eq!(stats.dirs_deleted, 1);
}
