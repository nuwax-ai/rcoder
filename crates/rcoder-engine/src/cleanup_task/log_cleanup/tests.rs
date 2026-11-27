//! 日志清理任务的单元测试：调度（周期、重试、停滞告警）和循环本身。挂载判定用假实现，不连
//! Docker。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::Instant;

use super::*;
use crate::cleanup_task::logs::testing::{FakeInventory, aged_dir};

/// 一个任务实例：日志目录在临时目录里，挂载判定是假的
struct Harness {
    root: TempDir,
    task: LogCleanupTask,
    inventory: Arc<FakeInventory>,
}

impl Harness {
    /// 检查间隔 5 分钟、周期一天，和默认配置一致
    fn new() -> Self {
        Self::with_intervals(
            Duration::from_secs(5 * 60),
            Duration::from_secs(24 * 60 * 60),
        )
    }

    fn with_intervals(check_interval: Duration, cleanup_interval: Duration) -> Self {
        let root = TempDir::new().unwrap();
        let inventory = FakeInventory::new(&[]);
        let task = LogCleanupTask::new(
            root.path().join("logs").to_str().unwrap().to_owned(),
            10,
            inventory.clone(),
            check_interval,
            cleanup_interval,
        );
        Self {
            root,
            task,
            inventory,
        }
    }

    fn log_dir(&self) -> PathBuf {
        self.root.path().join("logs")
    }

    /// 一次醒来：到期才清理
    async fn wake(&mut self) {
        self.task.run_if_due().await;
    }

    fn due(&self) -> bool {
        self.task.due()
    }

    /// 建一个 15 天前修改的过期目录
    fn aged(&self, name: &str) -> PathBuf {
        aged_dir(&self.log_dir(), name, 15)
    }

    /// 把"上一次完成"拨到一个周期之前
    fn make_due(&mut self) {
        self.task.last_cleanup =
            Instant::now().checked_sub(self.task.cleanup_interval + Duration::from_secs(1));
    }
}

fn entries(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}

async fn wait_until_gone(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{} was not cleaned up in time",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn log_cleanup_runs_only_when_its_own_interval_elapses() {
    let mut harness = Harness::new();

    // 第一次醒来：从未完成过，视为到期
    let first = harness.aged("computer-agent-runner-first-20261029035324");
    harness.wake().await;
    assert!(!first.exists(), "never having run counts as due");

    // 周期内：不扫日志目录，过期目录留着
    let second = harness.aged("computer-agent-runner-second-20261029035324");
    harness.wake().await;
    assert!(
        second.exists(),
        "a wake inside the interval must not scan the log directory"
    );

    // 周期到了：再扫一遍
    harness.make_due();
    harness.wake().await;
    assert!(
        !second.exists(),
        "an elapsed interval must run log cleanup again"
    );
}

/// 日志路径被一个普通文件占住时只告警、不算失败：这一轮照常记为已完成，不会变成每次醒来
/// 都重试。
#[tokio::test]
async fn a_log_path_that_is_not_a_directory_advances_the_schedule() {
    let mut harness = Harness::new();
    std::fs::write(harness.log_dir(), b"not a directory").unwrap();

    assert!(harness.due());
    harness.wake().await;
    assert!(!harness.due());
}

/// 一轮删不完（批次上限）的，下一次醒来接着删，不等一个完整周期。
#[tokio::test]
async fn an_incomplete_batch_continues_on_the_next_wake() {
    let mut harness = Harness::new();
    for index in 0..65 {
        harness.aged(&format!("runner-expired-{index}"));
    }

    harness.wake().await;
    assert_eq!(entries(&harness.log_dir()).len(), 1);
    assert!(
        harness.due(),
        "the rest is retried without waiting a full interval"
    );

    harness.wake().await;
    assert!(entries(&harness.log_dir()).is_empty());
    assert!(!harness.due());
}

#[cfg(unix)]
#[tokio::test]
async fn log_directory_lookup_errors_retry_after_the_path_is_repaired() {
    let mut harness = Harness::new();
    let log_dir = harness.log_dir();
    std::fs::create_dir_all(log_dir.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&log_dir, &log_dir).unwrap();

    harness.wake().await;
    assert!(harness.due());

    std::fs::remove_file(&log_dir).unwrap();
    let expired = harness.aged("runner-expired");
    harness.wake().await;
    assert!(!expired.exists());
    assert!(!harness.due());
}

#[tokio::test]
async fn the_interval_comparison_handles_the_full_config_range() {
    let mut harness = Harness::new();
    harness.task.last_cleanup = Some(Instant::now());

    harness.task.cleanup_interval = Duration::from_secs(u64::MAX);
    assert!(!harness.due());

    harness.task.cleanup_interval = Duration::ZERO;
    assert!(harness.due());
}

/// 挂载源持续"无法确认"：目录保留、周期不被重置，下一次扫描按日推进。
#[tokio::test]
async fn a_standing_unconfirmed_mount_does_not_reset_the_schedule() {
    let mut harness = Harness::new();
    let kept = harness.aged("runner-unconfirmed");
    harness.inventory.set_unconfirmed(&kept);

    harness.wake().await;

    assert!(kept.exists(), "an unconfirmable candidate must be kept");
    assert!(
        !harness.due(),
        "a standing unconfirmable mount must not reset the schedule"
    );
}

/// 清单暂时拿不到：下一次醒来接着做，周期不推进；恢复后这一轮就完成。
#[tokio::test]
async fn an_unavailable_inventory_retries_on_the_next_wake() {
    let mut harness = Harness::new();
    let expired = harness.aged("runner-expired");
    harness.inventory.set_failing(true);

    harness.wake().await;
    assert!(expired.exists());
    assert!(
        harness.due(),
        "an unavailable inventory must retry on the next wake"
    );

    harness.inventory.set_failing(false);
    harness.wake().await;
    assert!(!expired.exists());
    assert!(!harness.due());
}

/// 连续"要重试却没删掉任何东西"的轮数会被记下来（第 3 轮起告警）；一旦有进展就清零。
/// 顺延本身只记 debug，没有这个计数，一个卡住的候选会让清理无声地停在原地。
#[tokio::test]
async fn wakes_without_progress_are_counted_until_progress_resumes() {
    let mut harness = Harness::new();
    let expired = harness.aged("runner-expired");
    harness.inventory.set_failing(true);

    for sweep in 1..=4 {
        harness.wake().await;
        assert_eq!(
            harness.task.stalled_sweeps, sweep,
            "every wake that retries without deleting anything counts"
        );
        assert!(expired.exists());
    }

    // 清单恢复：这一轮删掉了东西，是进展
    harness.inventory.set_failing(false);
    harness.wake().await;
    assert!(!expired.exists());
    assert_eq!(harness.task.stalled_sweeps, 0);
}

/// 删掉了东西的顺延是进展，不算停滞：批次上限留下的目录下一次醒来接着删。
#[tokio::test]
async fn a_deferred_sweep_that_deleted_something_is_not_stalled() {
    let mut harness = Harness::new();
    for index in 0..65 {
        harness.aged(&format!("runner-{index:03}"));
    }

    harness.wake().await;
    assert!(harness.due(), "the rest is deferred to the next wake");
    assert_eq!(
        harness.task.stalled_sweeps, 0,
        "deleting a batch is progress even though the sweep is incomplete"
    );

    harness.wake().await;
    assert!(!harness.due());
    assert!(entries(&harness.log_dir()).is_empty());
}

/// 日志目录卡在初次扫描里（连 metadata 都不返回）时，扫描到时间上限就放弃这一轮；也不在
/// 下一次醒来马上重试——底层还没返回的阻塞调用仍占着线程。
#[tokio::test]
async fn a_hung_scan_is_abandoned_at_the_deadline_and_waits_for_the_next_interval() {
    let mut harness = Harness::new();
    let expired = harness.aged("runner-expired");
    harness
        .task
        .log_cleaner
        .set_scan_delay(Duration::from_secs(3));
    harness
        .task
        .log_cleaner
        .set_scan_timeout(Duration::from_millis(50));

    let started = std::time::Instant::now();
    harness.wake().await;

    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the wake must not wait for the hung scan"
    );
    assert!(
        !harness.due(),
        "a hung run must not be retried on the very next wake"
    );
    assert!(expired.exists());
}

/// 扫描的时间上限不管已经发起的目录删除：等到删除结束，这一轮才算做完、才推进周期。
#[tokio::test]
async fn a_slow_deletion_is_waited_for() {
    let mut harness = Harness::new();
    let expired = harness.aged("runner-expired");
    harness
        .task
        .log_cleaner
        .set_scan_timeout(Duration::from_millis(50));
    harness
        .task
        .log_cleaner
        .set_delete_delay(Duration::from_millis(400));

    harness.wake().await;

    assert!(!expired.exists(), "the deletion must run to the end");
    assert!(!harness.due(), "a completed run advances the schedule");
}

/// 循环按自己的节奏运行：启动后先跑一次，之后每次醒来检查是否到期（周期为 0 即每次都
/// 跑）；收到停止信号就退出，退出后不再清理。
#[tokio::test]
async fn the_task_runs_on_its_own_ticker_and_stops_on_shutdown() {
    let root = TempDir::new().unwrap();
    let log_dir = root.path().join("logs");
    let mut task = LogCleanupTask::new(
        log_dir.to_str().unwrap().to_owned(),
        10,
        FakeInventory::new(&[]),
        Duration::from_millis(20),
        Duration::ZERO,
    );
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let runner = tokio::spawn(async move {
        task.run(shutdown_rx).await;
    });

    // 启动后不等一个周期就跑
    let first = aged_dir(&log_dir, "runner-first", 15);
    wait_until_gone(&first).await;

    let second = aged_dir(&log_dir, "runner-second", 15);
    wait_until_gone(&second).await;

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap();

    let after = aged_dir(&log_dir, "runner-after-shutdown", 15);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(after.is_dir(), "a stopped task cleans nothing");
}

/// 检查间隔配成 0 不能让任务在启动时 panic。
#[tokio::test]
async fn a_zero_check_interval_is_accepted() {
    let mut task = LogCleanupTask::new(
        "/nonexistent/log-directory-for-test",
        10,
        FakeInventory::new(&[]),
        Duration::ZERO,
        Duration::from_secs(24 * 60 * 60),
    );
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task.run(shutdown_rx))
        .await
        .unwrap();
}
