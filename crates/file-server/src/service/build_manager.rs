//! Build 并发协调器：全局容量限制 + 同项目互斥，生命周期由 `AppState` 注入。

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::{Semaphore, SemaphorePermit};

use crate::error::{AppError, AppResult};

/// `start_after_release` 的同项目占用轮询间隔。guard 持有方是有限时长的
/// 构建异步 future（命令带超时），Drop 即释放；分钟级构建下 250ms 粒度
/// 足够，无需 per-project 通知状态。
const RELEASE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// 单次获取尝试结果。同项目互斥与全局容量满对调用方语义不同
/// （fail-fast 报错 vs 等待释放后重试），不能折叠成一个错误串。
enum Acquisition<'a> {
    Taken(BuildGuard<'a>),
    ProjectBusy,
    CapacityFull,
}

pub struct BuildManager {
    permits: Semaphore,
    projects: Mutex<HashSet<String>>,
}

impl BuildManager {
    pub fn new(max_concurrency: usize) -> Self {
        Self {
            permits: Semaphore::new(max_concurrency.max(1)),
            projects: Mutex::new(HashSet::new()),
        }
    }

    fn acquire<'a>(&'a self, project_id: &str) -> AppResult<Acquisition<'a>> {
        let mut projects = self
            .projects
            .lock()
            .map_err(|error| AppError::system(format!("build project lock poisoned: {error}")))?;
        if projects.contains(project_id) {
            return Ok(Acquisition::ProjectBusy);
        }
        // 对齐 nuwax：同项目互斥优先于全局容量判断，二者在同一临界区内完成，
        // 避免并发请求观察到不一致状态。
        let Ok(permit) = self.permits.try_acquire() else {
            return Ok(Acquisition::CapacityFull);
        };
        projects.insert(project_id.to_string());
        drop(projects);
        Ok(Acquisition::Taken(BuildGuard {
            project_id: project_id.to_string(),
            manager: self,
            _permit: permit,
        }))
    }

    pub fn try_start<'a>(&'a self, project_id: &str) -> AppResult<BuildGuard<'a>> {
        match self.acquire(project_id)? {
            Acquisition::Taken(guard) => Ok(guard),
            Acquisition::ProjectBusy => Err(AppError::business("This project is being built")),
            Acquisition::CapacityFull => Err(AppError::business(
                "Concurrency is full, please try again later",
            )),
        }
    }

    /// 同项目已有构建在途时等待其释放后获取（userapp 自动接替语义的等待半边）。
    ///
    /// 与 [`Self::try_start`] 的差异仅在 `ProjectBusy`：等待而非立即失败；
    /// 全局容量满仍立即报错（容量是跨项目共享资源，等待语义无法界定归属）。
    /// `budget` 内未释放则报错——正常持有方（构建 future）总会返回，超预算
    /// 说明持有方异常，fail-fast 优于无限等待。
    pub async fn start_after_release<'a>(
        &'a self,
        project_id: &str,
        budget: Duration,
    ) -> AppResult<BuildGuard<'a>> {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            match self.acquire(project_id)? {
                Acquisition::Taken(guard) => return Ok(guard),
                Acquisition::CapacityFull => {
                    return Err(AppError::business(
                        "Concurrency is full, please try again later",
                    ));
                }
                Acquisition::ProjectBusy => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(AppError::business(format!(
                            "another build for project '{project_id}' is still in flight after waiting {budget:?}"
                        )));
                    }
                    tokio::time::sleep(RELEASE_POLL_INTERVAL).await;
                }
            }
        }
    }
}

pub struct BuildGuard<'a> {
    project_id: String,
    manager: &'a BuildManager,
    _permit: SemaphorePermit<'a>,
}

impl Drop for BuildGuard<'_> {
    fn drop(&mut self) {
        let mut projects = match self.manager.projects.lock() {
            Ok(projects) => projects,
            Err(poisoned) => poisoned.into_inner(),
        };
        projects.remove(&self.project_id);
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

    /// 同项目占用释放后，等待方在预算内拿到 guard（自动接替的等待半边）。
    #[tokio::test]
    async fn start_after_release_acquires_once_in_flight_guard_drops() {
        use std::time::Duration;
        let manager = BuildManager::new(2);
        let first = manager.try_start("project").expect("first build guard");
        let waiter = manager.start_after_release("project", Duration::from_secs(5));
        let acquired = tokio::time::timeout(Duration::from_secs(2), async move {
            // 等待方先进入等待，再释放占用——验证的是等待而非先到先得
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(first);
            waiter.await
        })
        .await
        .expect("bounded wait")
        .expect("acquire after release");
        drop(acquired);
        assert!(manager.try_start("project").is_ok(), "guard fully released");
    }

    /// 占用方跨预算未释放 → 明确报错（持有方异常时 fail-fast，不无限等待）。
    #[tokio::test]
    async fn start_after_release_times_out_when_guard_never_drops() {
        use std::time::Duration;
        let manager = BuildManager::new(2);
        let _held = manager.try_start("project").expect("held guard");
        let error = match manager
            .start_after_release("project", Duration::from_millis(150))
            .await
        {
            Ok(_) => panic!("occupied project must not be acquired"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("still in flight"),
            "timeout error: {error}"
        );
    }
}
