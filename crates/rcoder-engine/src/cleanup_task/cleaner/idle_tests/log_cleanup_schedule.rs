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

/// 挂载源持续"无法确认"：目录保留、周期不被重置，下一次扫描按日推进。
#[tokio::test]
async fn a_standing_unconfirmed_mount_does_not_reset_the_schedule() {
    let mut fixture = Fixture::new(0).await;
    let log_dir = std::path::PathBuf::from(&fixture.cleaner.config.log_dir);
    let kept = aged_log_dir(&log_dir, "runner-unconfirmed");
    fixture
        .runtime
        .unconfirmed_paths
        .lock()
        .unwrap()
        .insert(kept.clone());

    fixture.cleaner.cleanup_once().await.unwrap();

    assert!(kept.exists(), "an unconfirmable candidate must be kept");
    assert!(
        !fixture.cleaner.log_cleanup_due(),
        "a standing unconfirmable mount must not reset the schedule"
    );
    fixture.close().await;
}

/// 清单暂时拿不到：下一轮清扫接着做，周期不推进；恢复后这一轮就完成。
#[tokio::test]
async fn an_unavailable_inventory_retries_on_the_next_sweep() {
    let mut fixture = Fixture::new(0).await;
    let log_dir = std::path::PathBuf::from(&fixture.cleaner.config.log_dir);
    let expired = aged_log_dir(&log_dir, "runner-expired");
    fixture
        .runtime
        .inventory_failing
        .store(true, Ordering::SeqCst);

    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(expired.exists());
    assert!(
        fixture.cleaner.log_cleanup_due(),
        "an unavailable inventory must retry on the next sweep"
    );

    fixture
        .runtime
        .inventory_failing
        .store(false, Ordering::SeqCst);
    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(!expired.exists());
    assert!(!fixture.cleaner.log_cleanup_due());
    fixture.close().await;
}

/// 连续"要重试却没删掉任何东西"的轮数会被记下来（第 3 轮起告警）；一旦有进展就清零。
/// 顺延本身只记 debug，没有这个计数，一个卡住的候选会让清理无声地停在原地。
#[tokio::test]
async fn sweeps_without_progress_are_counted_until_progress_resumes() {
    let mut fixture = Fixture::new(0).await;
    let log_dir = std::path::PathBuf::from(&fixture.cleaner.config.log_dir);
    let expired = aged_log_dir(&log_dir, "runner-expired");
    fixture
        .runtime
        .inventory_failing
        .store(true, Ordering::SeqCst);

    for sweep in 1..=4 {
        fixture.cleaner.cleanup_once().await.unwrap();
        assert_eq!(
            fixture.cleaner.log_cleanup_stalled_sweeps(),
            sweep,
            "every sweep that retries without deleting anything counts"
        );
        assert!(expired.exists());
    }

    // 清单恢复：这一轮删掉了东西，是进展
    fixture
        .runtime
        .inventory_failing
        .store(false, Ordering::SeqCst);
    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(!expired.exists());
    assert_eq!(fixture.cleaner.log_cleanup_stalled_sweeps(), 0);
    fixture.close().await;
}

/// 删掉了东西的顺延是进展，不算停滞：批次上限留下的目录下一轮接着删。
#[tokio::test]
async fn a_deferred_sweep_that_deleted_something_is_not_stalled() {
    let mut fixture = Fixture::new(0).await;
    let log_dir = std::path::PathBuf::from(&fixture.cleaner.config.log_dir);
    // 比一轮的批次上限多一个
    for index in 0..65 {
        aged_log_dir(&log_dir, &format!("runner-{index:03}"));
    }

    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(
        fixture.cleaner.log_cleanup_due(),
        "the rest is deferred to the next sweep"
    );
    assert_eq!(
        fixture.cleaner.log_cleanup_stalled_sweeps(),
        0,
        "deleting a batch is progress even though the sweep is incomplete"
    );

    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(!fixture.cleaner.log_cleanup_due());
    assert_eq!(
        std::fs::read_dir(&log_dir).unwrap().count(),
        0,
        "the deferred remainder is reclaimed next"
    );
    fixture.close().await;
}

/// 日志目录卡在初次扫描里（连 metadata 都不返回）时，扫描到时间上限就放弃这一轮，不把
/// 清扫任务一起停掉；也不在下一轮马上重试——底层还没返回的阻塞调用仍占着线程。
#[tokio::test]
async fn a_hung_log_cleanup_is_abandoned_at_the_deadline_and_waits_for_the_next_interval() {
    let mut fixture = Fixture::new(0).await;
    let log_dir = std::path::PathBuf::from(&fixture.cleaner.config.log_dir);
    let expired = aged_log_dir(&log_dir, "runner-expired");
    fixture
        .cleaner
        .log_cleaner
        .set_scan_delay(Duration::from_secs(3));
    fixture
        .cleaner
        .log_cleaner
        .set_scan_timeout(Duration::from_millis(50));

    let started = std::time::Instant::now();
    fixture.cleaner.cleanup_once().await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the sweep must not wait for the hung scan"
    );
    assert!(
        !fixture.cleaner.log_cleanup_due(),
        "a hung run must not be retried on the very next sweep"
    );
    assert!(expired.exists());
    fixture.close().await;
}

/// 扫描的时间上限不管已经发起的目录删除：等到删除结束，这一轮才算做完、才推进周期。
#[tokio::test]
async fn a_slow_deletion_is_waited_for() {
    let mut fixture = Fixture::new(0).await;
    let log_dir = std::path::PathBuf::from(&fixture.cleaner.config.log_dir);
    let expired = aged_log_dir(&log_dir, "runner-expired");
    fixture
        .cleaner
        .log_cleaner
        .set_scan_timeout(Duration::from_millis(50));
    fixture
        .cleaner
        .log_cleaner
        .set_delete_delay(Duration::from_millis(400));

    fixture.cleaner.cleanup_once().await.unwrap();

    assert!(!expired.exists(), "the deletion must run to the end");
    assert!(
        !fixture.cleaner.log_cleanup_due(),
        "a completed run advances the schedule"
    );
    fixture.close().await;
}
