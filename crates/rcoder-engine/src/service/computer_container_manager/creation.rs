//! Local admission follows the future's lifetime, never an elapsed marker TTL.
use crate::AppError;
use shared_types::{ErrorDetail, ServiceType, error_codes::ERR_CONTAINER_ADDRESS_NOT_READY};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Weak},
    time::Duration,
};
use tokio::sync::{Mutex, OwnedMutexGuard};

pub(super) const WAITER_BUDGET: Duration = Duration::from_secs(30);
type WorkloadKey = (String, String);
static CREATIONS: LazyLock<Mutex<HashMap<WorkloadKey, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn not_ready(message: &str) -> AppError {
    AppError::with_message(ERR_CONTAINER_ADDRESS_NOT_READY, message).with_error_detail(
        ErrorDetail::new(ERR_CONTAINER_ADDRESS_NOT_READY, "container.ensure", message)
            .with_retryable(true),
    )
}

pub(super) async fn acquire(
    service_type: &ServiceType,
    identifier: &str,
    deadline: tokio::time::Instant,
) -> Result<OwnedMutexGuard<()>, AppError> {
    let key = (
        service_type.container_family_key().to_owned(),
        identifier.to_owned(),
    );
    let lock = {
        let mut locks = CREATIONS.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            locks.insert(key, Arc::downgrade(&lock));
            lock
        }
    };
    tokio::time::timeout_at(deadline, lock.lock_owned())
        .await
        .map_err(|_| not_ready("Another request is still ensuring this container; retry later"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn live_creator_outlives_old_marker_ttl_without_handing_over_ownership() {
        let service = ServiceType::ComputerAgentRunner;
        let first = acquire(
            &service,
            "long-owner",
            tokio::time::Instant::now() + WAITER_BUDGET,
        )
        .await
        .unwrap();
        tokio::time::advance(Duration::from_secs(161)).await;
        let waiting = acquire(
            &service,
            "long-owner",
            tokio::time::Instant::now() + WAITER_BUDGET,
        );
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        // The same physical family shares admission across normal/computer APIs.
        let shared = acquire(
            &ServiceType::ComputerNormalProject,
            "long-owner",
            tokio::time::Instant::now() + WAITER_BUDGET,
        );
        tokio::pin!(shared);
        assert!(futures::poll!(&mut shared).is_pending());
        let independent = acquire(
            &service,
            "other-owner",
            tokio::time::Instant::now() + WAITER_BUDGET,
        )
        .await
        .unwrap();
        drop(independent);
        drop(first);
        let second = waiting.await.unwrap();
        drop(second);
        assert!(shared.await.is_ok());
    }

    #[tokio::test]
    async fn cancellation_releases_only_its_own_local_creation_guard() {
        let (held, acquired) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = acquire(
                &ServiceType::ComputerAgentRunner,
                "cancel-owner",
                tokio::time::Instant::now() + WAITER_BUDGET,
            )
            .await
            .unwrap();
            held.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        acquired.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let _next = acquire(
            &ServiceType::ComputerAgentRunner,
            "cancel-owner",
            tokio::time::Instant::now() + WAITER_BUDGET,
        )
        .await
        .unwrap();
    }
}
