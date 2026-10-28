//! A leader generation owns its actual tasks until every one has joined.
use super::SingleInstanceTaskHandles;
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::pin::Pin;
#[cfg(any(feature = "rcoder-pg", test))]
use std::time::Duration;
use tokio::{sync::broadcast, task::JoinHandle};

type TaskJoin = Pin<Box<dyn Future<Output = TaskExit> + Send>>;

struct TaskExit {
    name: &'static str,
    result: Result<(), tokio::task::JoinError>,
}

impl TaskExit {
    fn report(self, expected: bool) {
        match self.result {
            Ok(()) if expected => {
                tracing::debug!(task = self.name, "leader generation task joined")
            }
            Ok(()) => tracing::warn!(
                task = self.name,
                "leader generation task exited unexpectedly"
            ),
            Err(error) => {
                tracing::error!(task = self.name, %error, "leader generation task failed")
            }
        }
    }
}

pub(super) struct Generation {
    shutdown: broadcast::Sender<()>,
    tasks: FuturesUnordered<TaskJoin>,
}

impl Generation {
    pub(super) fn new(shutdown: broadcast::Sender<()>, handles: SingleInstanceTaskHandles) -> Self {
        let tasks = FuturesUnordered::new();
        let (cleanup, status, sync, recycle, orphan) = handles;
        for (name, handle) in [
            ("cleanup", cleanup),
            ("status checker", Some(status)),
            ("container sync", Some(sync)),
            ("userapp recycle", recycle),
            ("orphan reconciliation", orphan),
        ] {
            if let Some(handle) = handle {
                tasks.push(join_task(name, handle));
            }
        }
        Self { shutdown, tasks }
    }

    pub(super) async fn stop_and_drain(mut self) {
        let _ = self.shutdown.send(());
        while let Some(exit) = self.tasks.next().await {
            exit.report(true);
        }
    }
}

impl Drop for Generation {
    fn drop(&mut self) {
        // Cancellation is a request, never a claim that unfinished tasks joined.
        // Normal leadership/root shutdown always awaits stop_and_drain.
        let _ = self.shutdown.send(());
    }
}

fn join_task(name: &'static str, handle: JoinHandle<()>) -> TaskJoin {
    Box::pin(async move {
        TaskExit {
            name,
            result: handle.await,
        }
    })
}

#[cfg(any(feature = "rcoder-pg", test))]
enum Event {
    Shutdown,
    Exit(TaskExit),
    Tick,
}

/// The production caller supplies the election observation and the real task
/// factory. A generation is never replaced while any previous task is alive.
#[cfg(any(feature = "rcoder-pg", test))]
pub(super) async fn supervise<L, F, Fut>(
    is_leader: L,
    mut spawn: F,
    mut root_shutdown: broadcast::Receiver<()>,
    poll_interval: Duration,
) -> anyhow::Result<()>
where
    L: Fn() -> bool,
    F: FnMut(broadcast::Sender<()>) -> Fut,
    Fut: Future<Output = anyhow::Result<SingleInstanceTaskHandles>>,
{
    anyhow::ensure!(
        !poll_interval.is_zero(),
        "leader polling interval must be nonzero"
    );
    let mut current: Option<Generation> = None;
    let mut tick = tokio::time::interval(poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await;
    loop {
        let running = current.is_some();
        let event = tokio::select! {
            biased;
            _ = root_shutdown.recv() => Event::Shutdown,
            Some(exit) = async {
                match current.as_mut() {
                    Some(generation) => generation.tasks.next().await,
                    None => std::future::pending().await,
                }
            }, if running => Event::Exit(exit),
            _ = tick.tick() => Event::Tick,
        };
        match event {
            Event::Shutdown => {
                if let Some(generation) = current.take() {
                    generation.stop_and_drain().await;
                }
                return Ok(());
            }
            Event::Exit(exit) => {
                exit.report(false);
                if let Some(generation) = current.take() {
                    generation.stop_and_drain().await;
                }
                // A retained leader may restart only on a later polling tick.
            }
            Event::Tick => match (is_leader(), current.is_some()) {
                (true, false) => {
                    let (shutdown, _) = broadcast::channel(1);
                    let start = spawn(shutdown.clone());
                    tokio::pin!(start);
                    let mut revoked = false;
                    let mut shutting_down = false;
                    let result = loop {
                        tokio::select! {
                            biased;
                            _ = root_shutdown.recv(), if !shutting_down => {
                                shutting_down = true;
                                revoked = true;
                                let _ = shutdown.send(());
                            }
                            _ = tick.tick(), if !shutting_down => {
                                if !is_leader() && !revoked {
                                    revoked = true;
                                    let _ = shutdown.send(());
                                }
                            }
                            result = &mut start => break result,
                        }
                    };
                    match result {
                        Ok(handles) => {
                            let generation = Generation::new(shutdown, handles);
                            if revoked || shutting_down || !is_leader() {
                                // A later grant cannot adopt tasks from a
                                // startup whose leadership was revoked.
                                generation.stop_and_drain().await;
                            } else {
                                current = Some(generation);
                                tracing::info!(
                                    "[STORAGE_PG] leader: single-instance background tasks started"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::error!("[STORAGE_PG] leader task spawn failed: {error:#}")
                        }
                    }
                    if shutting_down {
                        return Ok(());
                    }
                }
                (false, true) => {
                    if let Some(generation) = current.take() {
                        generation.stop_and_drain().await;
                    }
                    tracing::info!(
                        "[STORAGE_PG] leader stepped down: single-instance background tasks stopped"
                    );
                }
                (true, true) | (false, false) => {}
            },
        }
    }
}
