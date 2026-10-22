//! Exercise real periodic task factories against the same AppState.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::{sync::Notify, task::AbortHandle};

#[derive(Default)]
struct Starts {
    groups: std::sync::Mutex<Vec<Vec<AbortHandle>>>,
    changed: Notify,
}

impl Starts {
    fn count(&self) -> usize {
        self.groups.lock().unwrap().len()
    }

    async fn wait_for(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self.count() >= count {
                    return;
                }
                self.changed.notified().await;
            }
        })
        .await
        .expect("actual generation task factory must run");
    }

    fn record(&self, handles: &SingleInstanceTaskHandles) {
        let (cleanup, status, sync, recycle) = handles;
        let actual = vec![
            cleanup.as_ref().expect("cleanup enabled").abort_handle(),
            status.abort_handle(),
            sync.abort_handle(),
            recycle.as_ref().expect("recycle enabled").abort_handle(),
        ];
        let mut groups = self.groups.lock().unwrap();
        if let Some(previous) = groups.last() {
            assert!(
                previous.iter().all(AbortHandle::is_finished),
                "old actual tasks must join before a replacement factory runs"
            );
        }
        groups.push(actual);
        drop(groups);
        self.changed.notify_one();
    }

    fn assert_all_joined(&self) {
        assert!(
            self.groups
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .all(AbortHandle::is_finished)
        );
    }
}

struct Fixture {
    state: Arc<AppState>,
    config: AppConfig,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let (state, _app, directory) = crate::userapp_builder::builder_access_test_state().await;
        let mut config = AppConfig::default();
        config.cleanup_config.enabled = true;
        config.cleanup_config.log_cleanup.log_dir =
            directory.path().join("logs").to_string_lossy().into_owned();
        config.userapp_recycle.enabled = true;
        Self {
            state,
            config,
            _directory: directory,
        }
    }

    async fn spawn(
        &self,
        shutdown: tokio::sync::broadcast::Sender<()>,
    ) -> anyhow::Result<SingleInstanceTaskHandles> {
        spawn_single_instance_tasks(
            &self.config,
            cleanup_runtime_config(&self.config),
            &self.state,
            &shutdown,
        )
        .await
    }
}

#[tokio::test]
async fn same_app_state_restarts_all_four_real_periodic_tasks() {
    let fixture = Fixture::new().await;
    for _ in 0..2 {
        let (shutdown, _) = tokio::sync::broadcast::channel(1);
        let handles = fixture.spawn(shutdown.clone()).await.unwrap();
        let starts = Starts::default();
        starts.record(&handles);
        generation::Generation::new(shutdown, handles)
            .stop_and_drain()
            .await;
        starts.assert_all_joined();
    }
}

#[tokio::test]
async fn leadership_reacquisition_restarts_real_tasks_after_the_old_generation_joins() {
    let fixture = Arc::new(Fixture::new().await);
    let leader = Arc::new(AtomicBool::new(true));
    let starts = Arc::new(Starts::default());
    let (shutdown, _) = tokio::sync::broadcast::channel(1);
    let observed_leader = leader.clone();
    let observed_starts = starts.clone();
    let mut supervisor = tokio::spawn(generation::supervise(
        move || observed_leader.load(Ordering::SeqCst),
        move |generation_shutdown| {
            let fixture = fixture.clone();
            let starts = observed_starts.clone();
            async move {
                let handles = fixture.spawn(generation_shutdown).await?;
                starts.record(&handles);
                Ok(handles)
            }
        },
        shutdown.subscribe(),
        Duration::from_millis(10),
    ));
    starts.wait_for(1).await;
    leader.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if starts.groups.lock().unwrap()[0]
                .iter()
                .all(AbortHandle::is_finished)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stepdown must join every actual task");
    leader.store(true, Ordering::SeqCst);
    starts.wait_for(2).await;
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut supervisor)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    starts.assert_all_joined();
}

#[tokio::test]
async fn losing_a_nonfirst_actual_task_cancels_and_joins_its_siblings_before_restart() {
    let fixture = Arc::new(Fixture::new().await);
    let starts = Arc::new(Starts::default());
    let (shutdown, _) = tokio::sync::broadcast::channel(1);
    let observed = starts.clone();
    let supervisor = tokio::spawn(generation::supervise(
        || true,
        move |generation_shutdown| {
            let fixture = fixture.clone();
            let starts = observed.clone();
            async move {
                let handles = fixture.spawn(generation_shutdown).await?;
                starts.record(&handles);
                Ok(handles)
            }
        },
        shutdown.subscribe(),
        Duration::from_millis(10),
    ));
    starts.wait_for(1).await;
    // status checker is the second task; a sequential cleanup.await would hide it.
    starts.groups.lock().unwrap()[0][1].abort();
    starts.wait_for(2).await;
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), supervisor)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    starts.assert_all_joined();
}

async fn blocked_drain_case(root_shutdown: bool) {
    let fixture = Arc::new(Fixture::new().await);
    let starts = Arc::new(Starts::default());
    let leader = Arc::new(AtomicBool::new(true));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (shutdown, _) = tokio::sync::broadcast::channel(1);
    let observed_starts = starts.clone();
    let observed_leader = leader.clone();
    let observed_entered = entered.clone();
    let observed_release = release.clone();
    let mut supervisor = tokio::spawn(generation::supervise(
        move || observed_leader.load(Ordering::SeqCst),
        move |generation_shutdown| {
            let fixture = fixture.clone();
            let starts = observed_starts.clone();
            let entered = observed_entered.clone();
            let release = observed_release.clone();
            async move {
                let (cleanup, status, sync, recycle) = fixture.spawn(generation_shutdown).await?;
                let sync = if starts.count() == 0 {
                    // Await the real sync task, then retain one explicit drain
                    // barrier. The supervisor must not mistake a signal for join.
                    tokio::spawn(async move {
                        sync.await.unwrap();
                        entered.notify_one();
                        release.notified().await;
                    })
                } else {
                    sync
                };
                let handles = (cleanup, status, sync, recycle);
                starts.record(&handles);
                Ok(handles)
            }
        },
        shutdown.subscribe(),
        Duration::from_millis(10),
    ));
    starts.wait_for(1).await;
    if root_shutdown {
        shutdown.send(()).unwrap();
    } else {
        leader.store(false, Ordering::SeqCst);
    }
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    leader.store(true, Ordering::SeqCst);
    assert_eq!(starts.count(), 1);
    assert!(
        !supervisor.is_finished(),
        "root/leadership shutdown cannot acknowledge an unjoined task"
    );
    release.notify_one();
    if !root_shutdown {
        starts.wait_for(2).await;
        shutdown.send(()).unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), &mut supervisor)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    starts.assert_all_joined();
}

#[tokio::test]
async fn quick_reacquisition_never_overlaps_an_unjoined_generation() {
    blocked_drain_case(false).await;
}

#[tokio::test]
async fn root_shutdown_waits_for_every_actual_generation_task() {
    blocked_drain_case(true).await;
}

#[tokio::test]
async fn leadership_loss_while_factory_is_pending_revokes_and_joins_its_actual_tasks() {
    let fixture = Arc::new(Fixture::new().await);
    let leader = Arc::new(AtomicBool::new(true));
    let starts = Arc::new(Starts::default());
    let release_factory = Arc::new(Notify::new());
    let (shutdown, _) = tokio::sync::broadcast::channel(1);
    let observed_leader = leader.clone();
    let observed_starts = starts.clone();
    let observed_release = release_factory.clone();
    let supervisor = tokio::spawn(generation::supervise(
        move || observed_leader.load(Ordering::SeqCst),
        move |generation_shutdown| {
            let fixture = fixture.clone();
            let starts = observed_starts.clone();
            let release = observed_release.clone();
            async move {
                let handles = fixture.spawn(generation_shutdown).await?;
                let first = starts.count() == 0;
                starts.record(&handles);
                if first {
                    release.notified().await;
                }
                Ok(handles)
            }
        },
        shutdown.subscribe(),
        Duration::from_millis(10),
    ));
    starts.wait_for(1).await;
    leader.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if starts.groups.lock().unwrap()[0]
                .iter()
                .all(AbortHandle::is_finished)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("leadership loss must stop the real tasks even while their factory is pending");
    leader.store(true, Ordering::SeqCst);
    assert_eq!(starts.count(), 1, "an unsettled factory cannot be replaced");
    release_factory.notify_one();
    starts.wait_for(2).await;
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), supervisor)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    starts.assert_all_joined();
}

#[tokio::test]
async fn background_drain_propagates_leader_shutdown_failure() {
    let handles = BackgroundTaskHandles {
        cleanup_handle: None,
        status_checker_handle: None,
        container_sync_handle: None,
        userapp_recycle_handle: None,
        pg_shadow_handle: None,
        pg_sync_handle: None,
        pg_leader_handle: Some(tokio::spawn(async {
            Err(anyhow::anyhow!("injected leader close failure"))
        })),
    };
    let error = handles
        .drain(tokio::time::Instant::now() + Duration::from_secs(5))
        .await
        .expect_err("an unconfirmed leader shutdown cannot report drained storage");
    assert!(error.to_string().contains("injected leader close failure"));
}
