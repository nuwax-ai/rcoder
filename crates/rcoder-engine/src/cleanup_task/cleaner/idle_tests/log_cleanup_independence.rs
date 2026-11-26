//! 日志清理和闲置回收是两个互不等待的循环：日志目录卡在文件系统上时，闲置容器的扫描和
//! 回收照常进行。调度本身的测试在 `cleanup_task/log_cleanup/tests.rs`。

use super::*;
use crate::cleanup_task::log_cleanup::LogCleanupTask;
use crate::cleanup_task::logs::testing::FakeInventory;
use std::path::PathBuf;
use std::time::Duration;

/// 建一个过期的日志目录（15 天前修改）
fn aged_log_dir(parent: &std::path::Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let old_time = std::time::SystemTime::now() - Duration::from_secs(15 * 24 * 60 * 60);
    std::fs::set_times(&dir, std::fs::FileTimes::new().set_modified(old_time)).unwrap();
    dir
}

/// 日志清理卡在扫描里不返回：闲置回收的一轮照常做完，两件事同时进行、互不等待。
#[tokio::test]
async fn a_hung_log_cleanup_does_not_hold_up_idle_reclaim() {
    let mut fixture = Fixture::new(0).await;
    let log_dir = PathBuf::from(&fixture.cleaner.config.log_dir);
    let mut log_cleanup = LogCleanupTask::new(
        log_dir.to_str().unwrap().to_owned(),
        10,
        FakeInventory::new(&[]),
        Duration::from_secs(5 * 60),
        Duration::from_secs(24 * 60 * 60),
    );
    let stuck = aged_log_dir(&log_dir, "runner-expired");
    log_cleanup
        .log_cleaner_mut()
        .set_scan_delay(Duration::from_secs(3));
    log_cleanup
        .log_cleaner_mut()
        .set_scan_timeout(Duration::from_millis(50));

    let started = std::time::Instant::now();
    let (idle_sweep, ()) = tokio::join!(fixture.cleaner.cleanup_once(), log_cleanup.run_if_due());

    idle_sweep.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "an idle sweep must not wait for the log cleanup: {:?}",
        started.elapsed()
    );
    assert!(stuck.is_dir());
    assert!(
        !log_cleanup.due(),
        "the hung scan is abandoned at its deadline and waits for the next interval"
    );
    fixture.close().await;
}
