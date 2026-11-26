//! 日志清理有自己的周期：容器清理任务每一轮只检查是否到期。
//!
//! 日志保留期以天计，跟着闲置容器回收的节奏（几分钟）反复扫描没有意义；每次扫描还会写
//! 一条 INFO 日志、读一遍日志目录。这里验证调度本身：第一次清扫到期，周期内之后的清扫不
//! 扫日志目录，周期到了再扫。
use super::*;

/// 建一个过期的日志目录（15 天前修改）
fn aged_log_dir(parent: &std::path::Path, name: &str) -> std::path::PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let old_time = std::time::SystemTime::now() - Duration::from_secs(15 * 24 * 60 * 60);
    std::fs::set_times(&dir, std::fs::FileTimes::new().set_modified(old_time)).unwrap();
    dir
}

#[tokio::test]
async fn log_cleanup_runs_only_when_its_own_interval_elapses() {
    let mut fixture = Fixture::new(0).await;
    let log_dir = std::path::PathBuf::from(&fixture.cleaner.config.log_dir);
    std::fs::create_dir_all(&log_dir).unwrap();

    // 第一次清扫：从未执行过，视为到期
    let first = aged_log_dir(&log_dir, "computer-agent-runner-first-20261029035324");
    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(!first.exists(), "the first sweep must run log cleanup");

    // 周期内：不扫日志目录，过期目录留着
    let second = aged_log_dir(&log_dir, "computer-agent-runner-second-20261029035324");
    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(
        second.exists(),
        "a sweep inside the interval must not scan the log directory"
    );

    // 周期到了：再扫一遍
    fixture.cleaner.last_log_cleanup = Some(
        tokio::time::Instant::now()
            - fixture.cleaner.config.log_cleanup_interval
            - Duration::from_secs(1),
    );
    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(
        !second.exists(),
        "an elapsed interval must run log cleanup again"
    );

    fixture.close().await;
}

/// 日志路径被一个普通文件占住时，清理器只告警、不算失败：这一轮照常记为已执行，不会
/// 因此变成每轮都重试。
#[tokio::test]
async fn a_log_path_that_is_not_a_directory_advances_the_schedule() {
    let mut fixture = Fixture::new(0).await;
    // 日志目录的位置被一个普通文件占住：读目录会失败
    let log_dir = std::path::PathBuf::from(&fixture.cleaner.config.log_dir);
    if let Some(parent) = log_dir.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&log_dir, b"not a directory").unwrap();

    assert!(fixture.cleaner.log_cleanup_due());
    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(!fixture.cleaner.log_cleanup_due());

    fixture.close().await;
}

#[tokio::test]
async fn log_cleanup_retries_on_the_next_sweep_after_mount_gate_contention() {
    let mut fixture = Fixture::new(0).await;
    let log_dir = std::path::PathBuf::from(&fixture.cleaner.config.log_dir);
    let expired = aged_log_dir(&log_dir, "runner-expired");
    let guard = docker_manager::try_begin_log_mount_cleanup().unwrap();

    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(expired.exists());
    drop(guard);

    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(!expired.exists());
    assert!(!fixture.cleaner.log_cleanup_due());
    fixture.close().await;
}

#[tokio::test]
async fn log_cleanup_continues_an_incomplete_batch_on_the_next_sweep() {
    let mut fixture = Fixture::new(0).await;
    let log_dir = std::path::PathBuf::from(&fixture.cleaner.config.log_dir);
    for index in 0..65 {
        aged_log_dir(&log_dir, &format!("runner-expired-{index}"));
    }

    fixture.cleaner.cleanup_once().await.unwrap();
    assert_eq!(std::fs::read_dir(&log_dir).unwrap().count(), 1);

    fixture.cleaner.cleanup_once().await.unwrap();
    assert_eq!(std::fs::read_dir(&log_dir).unwrap().count(), 0);
    assert!(!fixture.cleaner.log_cleanup_due());
    fixture.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn log_directory_lookup_errors_retry_after_the_path_is_repaired() {
    let mut fixture = Fixture::new(0).await;
    let log_dir = std::path::PathBuf::from(&fixture.cleaner.config.log_dir);
    std::os::unix::fs::symlink(&log_dir, &log_dir).unwrap();

    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(fixture.cleaner.log_cleanup_due());

    std::fs::remove_file(&log_dir).unwrap();
    let expired = aged_log_dir(&log_dir, "runner-expired");
    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(!expired.exists());
    assert!(!fixture.cleaner.log_cleanup_due());
    fixture.close().await;
}

#[tokio::test]
async fn log_cleanup_interval_comparison_handles_the_full_config_range() {
    let mut fixture = Fixture::new(0).await;
    fixture.cleaner.last_log_cleanup = Some(tokio::time::Instant::now());

    fixture.cleaner.config.log_cleanup_interval = Duration::from_secs(u64::MAX);
    assert!(!fixture.cleaner.log_cleanup_due());

    fixture.cleaner.config.log_cleanup_interval = Duration::ZERO;
    assert!(fixture.cleaner.log_cleanup_due());
    fixture.close().await;
}
