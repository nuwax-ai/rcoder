//! Build 并发协调器：全局容量限制 + 同项目互斥，生命周期由 `AppState` 注入。

use std::collections::HashSet;
use std::sync::Mutex;

use tokio::sync::{Semaphore, SemaphorePermit};

use crate::error::{AppError, AppResult};

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

    pub fn try_start<'a>(&'a self, project_id: &str) -> AppResult<BuildGuard<'a>> {
        let mut projects = self
            .projects
            .lock()
            .map_err(|error| AppError::system(format!("build project lock poisoned: {error}")))?;
        if projects.contains(project_id) {
            return Err(AppError::business("This project is being built"));
        }
        // 对齐 nuwax：同项目互斥优先于全局容量判断，二者在同一临界区内完成，
        // 避免并发请求观察到不一致状态。
        let permit = self
            .permits
            .try_acquire()
            .map_err(|_| AppError::business("Concurrency is full, please try again later"))?;
        projects.insert(project_id.to_string());
        drop(projects);
        Ok(BuildGuard {
            project_id: project_id.to_string(),
            manager: self,
            _permit: permit,
        })
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
}
