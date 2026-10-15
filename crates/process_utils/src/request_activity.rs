//! Non-exclusive observation of actual HTTP work and response bodies.
//! Unlike WorkerRegistry, dropping an observer does not mark a write unknown.
use std::sync::atomic::{AtomicUsize, Ordering};

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

#[must_use]
pub struct RequestActivity {
    _private: (),
}

impl RequestActivity {
    pub fn begin() -> Self {
        ACTIVE.fetch_add(1, Ordering::AcqRel);
        Self { _private: () }
    }
}

impl Drop for RequestActivity {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn active_in_process() -> usize {
    ACTIVE.load(Ordering::Acquire)
}
