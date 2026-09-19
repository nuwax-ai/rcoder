//! Cancellation-safe admission/drain boundary shared by HTTP and detached workers.
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
const CLOSING: usize = 1 << (usize::BITS - 1);
#[derive(Default)]
pub struct OperationFlightGate {
    state: AtomicUsize,
}
#[derive(Debug, thiserror::Error)]
#[error("service is shutting down; new operations are not admitted")]
pub struct FlightAdmissionClosed;
pub struct FlightGuard {
    gate: Arc<OperationFlightGate>,
}
impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.gate.state.fetch_sub(1, Ordering::AcqRel);
    }
}
impl OperationFlightGate {
    pub fn guard(self: &Arc<Self>) -> Result<FlightGuard, FlightAdmissionClosed> {
        self.state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                if state & CLOSING != 0 || state == CLOSING - 1 {
                    None
                } else {
                    Some(state + 1)
                }
            })
            .map_err(|_| FlightAdmissionClosed)?;
        Ok(FlightGuard { gate: self.clone() })
    }
    pub fn close(&self) {
        self.state.fetch_or(CLOSING, Ordering::AcqRel);
    }
    pub fn active(&self) -> usize {
        self.state.load(Ordering::Acquire) & !CLOSING
    }
    pub async fn wait_idle(&self, budget: Duration) -> usize {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let active = self.active();
            if active == 0 || tokio::time::Instant::now() >= deadline {
                return active;
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + Duration::from_millis(10)),
            )
            .await;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn closing_tracks_unpolled_worker_and_rejects_new_admission() {
        let gate = Arc::new(OperationFlightGate::default());
        let held = gate.guard().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            let _held = held;
            let _ = rx.await;
        });
        gate.close();
        assert!(gate.guard().is_err());
        assert_eq!(gate.wait_idle(Duration::from_millis(1)).await, 1);
        tx.send(()).unwrap();
        worker.await.unwrap();
        assert_eq!(gate.wait_idle(Duration::from_secs(1)).await, 0);
        assert!(gate.guard().is_err());
    }
}
