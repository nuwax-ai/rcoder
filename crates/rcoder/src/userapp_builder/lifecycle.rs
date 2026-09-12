//! Local creation/registration and deletion use the same cancellation-safe lease.
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Weak},
};
use tokio::sync::{Mutex, OwnedMutexGuard};
static LOCKS: LazyLock<Mutex<HashMap<String, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
pub(super) async fn acquire(app_id: &str) -> OwnedMutexGuard<()> {
    let lock = {
        let mut locks = LOCKS.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        match locks.get(app_id).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(Mutex::new(()));
                locks.insert(app_id.into(), Arc::downgrade(&lock));
                lock
            }
        }
    };
    lock.lock_owned().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cleanup_blocks_same_app_creation_until_ticket_released() {
        let old = acquire("receipt-serialization-test").await;
        let next = acquire("receipt-serialization-test");
        tokio::pin!(next);
        assert!(futures::poll!(&mut next).is_pending());
        let unrelated = acquire("unrelated-receipt-test").await;
        drop(unrelated);
        drop(old);
        let _new = tokio::time::timeout(std::time::Duration::from_secs(1), next)
            .await
            .expect("replacement must resume after old cleanup releases");
    }
}
