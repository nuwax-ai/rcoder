//! Admission and completion tracking for one embedded host's local workers.
use crate::command_context::{CommandContext, CommandRecord, WorkIdentity};
use std::{
    future::Future,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

struct Gate {
    closed: bool,
    active: usize,
    unconfirmed: bool,
}
pub struct WorkerRegistry {
    gate: Mutex<Gate>,
    changed: tokio::sync::Notify,
    cancellation: CancellationToken,
    root: Option<PathBuf>,
    cleanup_pending: Arc<std::sync::atomic::AtomicUsize>,
}
struct Permit {
    registry: Arc<WorkerRegistry>,
    completed: bool,
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut gate) = self.registry.gate.lock() {
            gate.active -= 1;
            gate.unconfirmed |= !self.completed;
        }
        self.registry.changed.notify_waiters();
    }
}
impl WorkerRegistry {
    pub fn new(root: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            gate: Mutex::new(Gate {
                closed: false,
                active: 0,
                unconfirmed: false,
            }),
            changed: tokio::sync::Notify::new(),
            cancellation: CancellationToken::new(),
            cleanup_pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            root,
        })
    }
    pub fn spawn<F>(
        self: &Arc<Self>,
        future: F,
    ) -> Result<tokio::task::JoinHandle<Result<F::Output, String>>, String>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let identity = CommandContext::current()
            .map(|c| c.identity)
            .unwrap_or_default();
        self.spawn_identified(identity, future)
    }
    pub fn spawn_identified<F>(
        self: &Arc<Self>,
        identity: WorkIdentity,
        future: F,
    ) -> Result<tokio::task::JoinHandle<Result<F::Output, String>>, String>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let mut gate = self
            .gate
            .lock()
            .map_err(|_| "worker admission lock poisoned")?;
        if gate.closed {
            return Err("embedded runtime is stopping".into());
        }
        // Receipt and tracking registration precede spawn/202 under the same gate.
        let record = self
            .root
            .as_ref()
            .map(|r| CommandRecord::prepare_identified(r.join("workers"), identity.clone()))
            .transpose()
            .map_err(|e| format!("persist worker admission: {e}"))?;
        gate.active += 1;
        let permit = Permit {
            registry: self.clone(),
            completed: false,
        };
        let parent_cancellation = CommandContext::current().map(|c| c.cancellation);
        let cancellation = self.cancellation.child_token();
        let linked_cancellation = cancellation.clone();
        let context = CommandContext {
            identity,
            cancellation,
            journal_root: self.root.as_ref().map(|r| r.join("commands")),
            cleanup_pending: self.cleanup_pending.clone(),
        };
        drop(gate);
        Ok(tokio::spawn(context.scope(async move {
            let mut permit = permit;
            tokio::pin!(future);
            let output = if let Some(parent) = parent_cancellation {
                tokio::select! {
                    output = &mut future => output,
                    () = parent.cancelled() => { linked_cancellation.cancel(); future.await }
                }
            } else {
                future.await
            };
            if let Some(record) = record {
                record
                    .quiescent()
                    .map_err(|e| format!("persist worker completion: {e}"))?;
            }
            permit.completed = true;
            Ok(output)
        })))
    }
    pub fn cleanup_pending(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        self.cleanup_pending.clone()
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.child_token()
    }
    pub fn close(&self) -> Result<(), String> {
        let mut gate = self
            .gate
            .lock()
            .map_err(|_| "worker admission lock poisoned")?;
        gate.closed = true;
        self.cancellation.cancel();
        Ok(())
    }
    pub async fn drain(&self, deadline: tokio::time::Instant) -> Result<(), String> {
        self.close()?;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (active, unconfirmed) = {
                let gate = self.gate.lock().map_err(|_| "worker gate poisoned")?;
                (gate.active, gate.unconfirmed)
            };
            if active == 0 {
                if unconfirmed {
                    return Err("embedded worker completion unconfirmed".into());
                }
                break;
            }
            tokio::time::timeout_at(deadline, notified)
                .await
                .map_err(|_| "embedded workers remain active; cleanup unconfirmed")?;
        }
        if self
            .cleanup_pending
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
        {
            return Err("owned command cleanup remains unconfirmed".into());
        }
        if let Some(root) = &self.root {
            crate::command_context::require_quiescent(&root.join("workers"))
                .map_err(|e| e.to_string())?;
            crate::command_context::require_quiescent(&root.join("commands"))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn closing_gate_rejects_new_workers_and_waits_for_accepted_blocking_work() {
        let root = tempfile::tempdir().unwrap();
        let registry = WorkerRegistry::new(Some(root.path().into()));
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let worker = registry
            .spawn(async move {
                tokio::task::spawn_blocking(move || {
                    started.send(()).unwrap();
                    released.recv().unwrap();
                })
                .await
                .unwrap();
            })
            .unwrap();
        ready.await.unwrap();
        registry.close().unwrap();
        assert!(registry.spawn(async {}).is_err());
        assert!(
            registry
                .drain(tokio::time::Instant::now() + std::time::Duration::from_millis(30))
                .await
                .is_err()
        );
        release.send(()).unwrap();
        worker.await.unwrap().unwrap();
        registry
            .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn lost_observer_does_not_abort_worker_and_panic_keeps_durable_protection() {
        let root = tempfile::tempdir().unwrap();
        let registry = WorkerRegistry::new(Some(root.path().into()));
        let (release, released) = tokio::sync::oneshot::channel();
        drop(
            registry
                .spawn(async move {
                    released.await.unwrap();
                })
                .unwrap(),
        );
        registry.close().unwrap();
        assert!(
            registry
                .drain(tokio::time::Instant::now() + std::time::Duration::from_millis(30))
                .await
                .is_err()
        );
        release.send(()).unwrap();
        registry
            .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
            .await
            .unwrap();
        let broken = WorkerRegistry::new(Some(root.path().join("panic")));
        let worker = broken
            .spawn(async {
                panic!("injected worker panic");
            })
            .unwrap();
        assert!(worker.await.is_err());
        assert!(
            broken
                .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn close_cancels_worker_even_when_parent_context_remains_live() {
        let registry = WorkerRegistry::new(None);
        let parent = CancellationToken::new();
        let spawned = registry.clone();
        let (worker,) = CommandContext {
            identity: WorkIdentity::default(),
            cancellation: parent.clone(),
            journal_root: None,
            cleanup_pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
        .scope(async move {
            (spawned
                .spawn(async {
                    CommandContext::current()
                        .unwrap()
                        .cancellation
                        .cancelled()
                        .await;
                })
                .unwrap(),)
        })
        .await;
        registry.close().unwrap();
        registry
            .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
            .await
            .unwrap();
        worker.await.unwrap().unwrap();
        assert!(!parent.is_cancelled());
    }
    #[tokio::test]
    async fn memory_only_registry_preserves_panicked_worker_protection() {
        let registry = WorkerRegistry::new(None);
        assert!(
            registry
                .spawn(async {
                    panic!("injected panic");
                })
                .unwrap()
                .await
                .is_err()
        );
        assert!(
            registry
                .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn memory_only_command_cleanup_blocks_drain_until_confirmed() {
        let registry = WorkerRegistry::new(None);
        let worker = registry
            .spawn(async { CommandRecord::prepare().unwrap().unwrap() })
            .unwrap();
        let record = worker.await.unwrap().unwrap();
        assert!(
            registry
                .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
                .await
                .is_err()
        );
        record.quiescent().unwrap();
        registry
            .drain(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
            .await
            .unwrap();
    }
}
