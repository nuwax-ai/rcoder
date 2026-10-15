//! 同一 project 的物理执行转换串行；不同 project 仍独立并行。

use std::sync::{Arc, LazyLock, Weak};

use dashmap::{DashMap, mapref::entry::Entry};
use tokio::sync::{Mutex, OwnedMutexGuard};

static TRANSITIONS: LazyLock<DashMap<String, Weak<Mutex<()>>>> = LazyLock::new(DashMap::new);

/// 停旧、清理和创建的同步边界。不能在持有 DashMap guard 时等待。
#[derive(Debug)]
pub struct ExecutionTransitionGuard {
    project_id: String,
    guard: Option<OwnedMutexGuard<()>>,
}

pub async fn lock_execution_transition(project_id: &str) -> ExecutionTransitionGuard {
    let mutex = match TRANSITIONS.entry(project_id.to_string()) {
        Entry::Occupied(mut entry) => match entry.get().upgrade() {
            Some(mutex) => mutex,
            None => {
                let mutex = Arc::new(Mutex::new(()));
                entry.insert(Arc::downgrade(&mutex));
                mutex
            }
        },
        Entry::Vacant(entry) => {
            let mutex = Arc::new(Mutex::new(()));
            entry.insert(Arc::downgrade(&mutex));
            mutex
        }
    };
    ExecutionTransitionGuard {
        project_id: project_id.to_string(),
        guard: Some(mutex.lock_owned().await),
    }
}

impl Drop for ExecutionTransitionGuard {
    fn drop(&mut self) {
        // 先释放 mutex 再清 Weak；已有等待者保有 Arc 时不能移除这把锁。
        self.guard.take();
        TRANSITIONS.remove_if(&self.project_id, |_, mutex| mutex.strong_count() == 0);
    }
}
