use super::*;
use std::{
    collections::HashMap,
    sync::{Mutex, Weak},
};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

#[derive(Default)]
pub(crate) struct AdmissionMap {
    pub(crate) owner: Arc<()>,
    locks: Mutex<HashMap<WorkloadKey, Weak<RwLock<()>>>>,
}
enum Lease {
    Shared(#[allow(dead_code)] OwnedRwLockReadGuard<()>),
    Exclusive(#[allow(dead_code)] OwnedRwLockWriteGuard<()>),
}
pub(super) struct MemoryGuard {
    store: Arc<crate::ProjectAdapter>,
    _lease: Lease,
}
pub(super) async fn acquire(
    store: Arc<crate::ProjectAdapter>,
    key: WorkloadKey,
    mode: WorkloadAdmissionMode,
    deadline: Instant,
) -> Result<WorkloadAdmissionGuard> {
    ensure!(
        Instant::now() < deadline,
        "Workload admission deadline expired"
    );
    let lock = {
        let mut entries = store
            .workload_admission
            .locks
            .lock()
            .map_err(|_| anyhow::anyhow!("Workload admission map poisoned"))?;
        entries.retain(|_, entry| entry.strong_count() > 0);
        if let Some(existing) = entries.get(&key).and_then(Weak::upgrade) {
            existing
        } else {
            let lock = Arc::new(RwLock::new(()));
            entries.insert(key.clone(), Arc::downgrade(&lock));
            lock
        }
    };
    let lease = tokio::time::timeout_at(deadline, async {
        match mode {
            WorkloadAdmissionMode::Shared => Lease::Shared(lock.read_owned().await),
            WorkloadAdmissionMode::Exclusive => Lease::Exclusive(lock.write_owned().await),
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("Workload admission deadline expired"))?;
    Ok(WorkloadAdmissionGuard {
        key,
        mode,
        owner: store.workload_admission.owner.clone(),
        inner: Guard::Memory(MemoryGuard {
            store,
            _lease: lease,
        }),
    })
}
impl MemoryGuard {
    pub(super) fn has_references(&self, key: &WorkloadKey) -> bool {
        self.store.workload_has_references(key)
    }
    pub(super) fn capture(
        &self,
        key: &WorkloadKey,
        owner: Arc<()>,
        name: &str,
    ) -> Result<WorkloadRegistrationFrame> {
        self.store.capture_workload_frame(key, owner, name)
    }
    pub(super) fn retire(
        &self,
        frame: &WorkloadRegistrationFrame,
    ) -> Result<PersistenceWriteOutcome> {
        self.store.retire_workload_frame(frame)
    }
}
