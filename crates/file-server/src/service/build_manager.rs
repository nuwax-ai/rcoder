//! Build 并发协调器：全局容量限制 + 同项目互斥，生命周期由 AppState 注入。

use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};

enum Acquisition {
    Taken(BuildGuard),
    ProjectBusy,
    CapacityFull,
}

struct Inner {
    permits: Arc<Semaphore>,
    projects: Mutex<HashSet<String>>,
    released: Notify,
}

pub struct BuildManager {
    inner: Arc<Inner>,
}

impl BuildManager {
    pub fn new(max_concurrency: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                permits: Arc::new(Semaphore::new(max_concurrency.max(1))),
                projects: Mutex::new(HashSet::new()),
                released: Notify::new(),
            }),
        }
    }

    fn acquire(&self, project_id: &str) -> AppResult<Acquisition> {
        let mut projects =
            self.inner.projects.lock().map_err(|error| {
                AppError::system(format!("build project lock poisoned: {error}"))
            })?;
        if projects.contains(project_id) {
            return Ok(Acquisition::ProjectBusy);
        }
        let Ok(permit) = self.inner.permits.clone().try_acquire_owned() else {
            return Ok(Acquisition::CapacityFull);
        };
        projects.insert(project_id.to_string());
        Ok(Acquisition::Taken(BuildGuard {
            lease: Arc::new(BuildLease {
                project_id: project_id.to_owned(),
                manager: self.inner.clone(),
                permit: Some(permit),
                cleanup_pending: std::sync::atomic::AtomicBool::new(false),
            }),
            retained: None,
        }))
    }

    /// 普通项目仍立即拒绝重复构建及容量不足。
    pub fn try_start(&self, project_id: &str) -> AppResult<BuildGuard> {
        match self.acquire(project_id)? {
            Acquisition::Taken(guard) => Ok(guard),
            Acquisition::ProjectBusy => Err(AppError::business("This project is being built")),
            Acquisition::CapacityFull => Err(AppError::business(
                "Concurrency is full, please try again later",
            )),
        }
    }

    /// 有界等待同项目和全局容量；取消立即退出。预算是调用方的等待政策，
    /// 不是旧任务故障判据。零预算只立即尝试一次，不进入等待。
    pub async fn start_after_release(
        &self,
        project_id: &str,
        budget: Duration,
    ) -> AppResult<BuildGuard> {
        let cancellation = process_utils::command_context::CommandContext::current()
            .map(|context| context.cancellation)
            .unwrap_or_default();
        self.wait_for_slot(project_id, budget, &cancellation).await
    }

    async fn wait_for_slot(
        &self,
        project_id: &str,
        budget: Duration,
        cancellation: &CancellationToken,
    ) -> AppResult<BuildGuard> {
        let deadline = tokio::time::Instant::now()
            .checked_add(budget)
            .ok_or_else(|| AppError::validation("build wait budget is too large"))?;
        let timeout = || {
            AppError::business(format!(
                "build slot wait timed out for project '{project_id}' after {budget:?}; no build command was started"
            ))
        };
        let mut first_attempt = true;
        loop {
            // Register before checking availability so release cannot be missed.
            let released = self.inner.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if cancellation.is_cancelled() {
                return Err(AppError::business(
                    "build cancelled while waiting for a slot",
                ));
            }
            if !first_attempt && tokio::time::Instant::now() >= deadline {
                return Err(timeout());
            }
            if let Acquisition::Taken(guard) = self.acquire(project_id)? {
                if !budget.is_zero() && tokio::time::Instant::now() >= deadline {
                    return Err(timeout());
                }
                return Ok(guard);
            }
            first_attempt = false;
            tokio::select! {
                biased;
                () = cancellation.cancelled() => {
                    return Err(AppError::business("build cancelled while waiting for a slot"));
                }
                () = tokio::time::sleep_until(deadline) => return Err(timeout()),
                () = &mut released => {}
            }
        }
    }
}

/// Clones retain one lease, never acquire another permit. A command whose tree
/// cleanup is pending keeps the lease until its retained cleanup worker finishes.
#[derive(Clone)]
pub struct BuildGuard {
    // Release the activity lease before publishing slot availability.
    retained: Option<Arc<dyn Send + Sync>>,
    lease: Arc<BuildLease>,
}

tokio::task_local! { static CURRENT_BUILD: BuildGuard; }

impl BuildGuard {
    /// Also retain the caller's workspace read lease during deferred cleanup.
    pub fn keep_alive<T: Send + Sync + 'static>(mut self, resource: Arc<T>) -> Self {
        self.retained = Some(resource);
        self
    }

    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        CURRENT_BUILD.scope(self.clone(), future).await
    }

    pub(crate) fn current() -> Option<Self> {
        CURRENT_BUILD.try_with(Clone::clone).ok()
    }

    pub(crate) fn cleanup_pending(&self) -> bool {
        self.lease
            .cleanup_pending
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn retain_command_cleanup(
        child: process_utils::guardian::OwnedChild,
        record: Option<process_utils::command_context::CommandRecord>,
    ) {
        let guard = Self::current();
        if let Some(guard) = &guard {
            // Stop this operation's retry/self-heal loop too. A fresh lease is
            // allowed as soon as this concrete child's cleanup releases it.
            guard
                .lease
                .cleanup_pending
                .store(true, std::sync::atomic::Ordering::Release);
        }
        process_utils::command_context::retain_cleanup_with_resource(Some(child), record, guard);
    }

    pub fn project_id(&self) -> &str {
        &self.lease.project_id
    }
}

struct BuildLease {
    project_id: String,
    manager: Arc<Inner>,
    permit: Option<OwnedSemaphorePermit>,
    cleanup_pending: std::sync::atomic::AtomicBool,
}

impl Drop for BuildLease {
    fn drop(&mut self) {
        let mut projects = match self.manager.projects.lock() {
            Ok(projects) => projects,
            Err(poisoned) => poisoned.into_inner(),
        };
        projects.remove(&self.project_id);
        // Return capacity before exposing an unoccupied project.
        drop(self.permit.take());
        drop(projects);
        self.manager.released.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_project_and_releases_with_guard() {
        let manager = BuildManager::new(2);
        let first = manager.try_start("project").expect("first build guard");
        assert!(matches!(
            manager.try_start("project"),
            Err(AppError::Business(_))
        ));
        drop(first);
        assert!(manager.try_start("project").is_ok());
    }

    #[test]
    fn rejects_when_global_capacity_is_full() {
        let manager = BuildManager::new(1);
        let _first = manager.try_start("one").expect("first build guard");
        assert!(matches!(
            manager.try_start("two"),
            Err(AppError::Business(_))
        ));
    }

    #[test]
    fn duplicate_project_error_takes_priority_when_capacity_is_full() {
        let manager = BuildManager::new(1);
        let _first = manager.try_start("same").expect("first build guard");
        let error = match manager.try_start("same") {
            Ok(_) => panic!("duplicate build must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("This project is being built"));
    }

    async fn assert_pending<F: Future>(future: std::pin::Pin<&mut F>) {
        let mut future = future;
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn start_after_release_waits_for_project_and_competing_capacity() {
        let manager = BuildManager::new(1);
        let first = manager.try_start("project").unwrap();
        let waiter = manager.start_after_release("project", Duration::from_secs(2));
        tokio::pin!(waiter);
        assert_pending(waiter.as_mut()).await;
        drop(first);
        // Another project takes capacity before this registered waiter is polled.
        let competitor = manager.try_start("other").unwrap();
        assert_pending(waiter.as_mut()).await;
        drop(competitor);
        let acquired = tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .unwrap()
            .unwrap();
        drop(acquired);
        assert!(manager.try_start("project").is_ok());
    }

    #[tokio::test]
    async fn start_after_release_enforces_deadline_and_cancellation() {
        let manager = BuildManager::new(1);
        let immediate = manager
            .start_after_release("project", Duration::ZERO)
            .await
            .unwrap();
        assert!(
            manager
                .start_after_release("project", Duration::ZERO)
                .await
                .is_err()
        );
        let expired = manager.start_after_release("project", Duration::from_millis(20));
        tokio::pin!(expired);
        assert_pending(expired.as_mut()).await;
        // Intentionally don't poll the timer while releasing after its deadline.
        tokio::time::sleep(Duration::from_millis(40)).await;
        drop(immediate);
        assert!(
            expired.await.is_err(),
            "release after deadline cannot turn timeout into success"
        );
        let occupied = manager.try_start("project").unwrap();
        let cancellation = CancellationToken::new();
        let waiting = manager.wait_for_slot("project", Duration::from_secs(600), &cancellation);
        tokio::pin!(waiting);
        assert_pending(waiting.as_mut()).await;
        cancellation.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), waiting)
                .await
                .unwrap()
                .is_err()
        );
        assert!(
            manager.try_start("project").is_err(),
            "cancelling waiter must not release holder"
        );
        drop(occupied);
        assert!(manager.try_start("project").is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deferred_tree_cleanup_retains_only_its_workspace_until_confirmed() {
        use process_utils::guardian::OwnedChild;
        let root = tempfile::tempdir().unwrap();
        let command_root = root.path().join("instance/guardians/command");
        std::fs::create_dir_all(&command_root).unwrap();
        let mut receipt = serde_json::json!({
            "version": 2, "id": "command", "instance_id": "instance",
            "phase": "Running", "command_record": null, "command_digest": "test",
            "root_status": 0, "diagnostic_pid": null
        });
        std::fs::write(command_root.join("receipt.json"), receipt.to_string()).unwrap();
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        child.wait().await.unwrap();
        let mut owned = OwnedChild::Guarded {
            child: Box::new(child),
            lease: None,
            root: command_root.clone(),
            receipt_unavailable_since: None,
        };
        assert!(matches!(
            owned.stop(Duration::ZERO).await,
            process_utils::managed_tree::StopOutcome::Unconfirmed
        ));
        let manager = BuildManager::new(2);
        let activity = Arc::new(tokio::sync::RwLock::new(()));
        let guard = manager
            .try_start("project")
            .unwrap()
            .keep_alive(Arc::new(activity.clone().read_owned().await));
        guard
            .scope(async {
                BuildGuard::retain_command_cleanup(owned, None);
            })
            .await;
        assert!(guard.cleanup_pending());
        drop(guard);
        assert!(manager.try_start("project").is_err());
        assert!(activity.clone().try_write_owned().is_err());
        assert!(manager.try_start("unrelated").is_ok());
        receipt["phase"] = "Quiescent".into();
        std::fs::write(command_root.join("receipt.json"), receipt.to_string()).unwrap();
        let next = manager
            .start_after_release("project", Duration::from_secs(2))
            .await
            .unwrap();
        assert!(!next.cleanup_pending());
        assert!(
            activity.try_write_owned().is_ok(),
            "old workspace lease must also be released"
        );
        drop(next);
    }
}
