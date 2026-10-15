use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, LazyLock, Mutex, Weak},
    time::Duration,
};

use crate::error::{AppError, AppResult};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Point {
    Preserve,
    RenameCapture,
    ReceiptCheck,
}

type GateRegistry = HashMap<(PathBuf, Point), Weak<Gate>>;

static GATES: LazyLock<Mutex<GateRegistry>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) struct Gate {
    entered: tokio::sync::Notify,
    released: Mutex<bool>,
    wake: Condvar,
}

pub(super) struct Probe(Arc<Gate>);

impl Probe {
    pub(super) async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(20), self.0.entered.notified())
            .await
            .expect("workspace worker entered preservation gate");
    }

    pub(super) fn release(&self) {
        *self.0.released.lock().unwrap() = true;
        self.0.wake.notify_all();
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.release();
    }
}

pub(super) fn register(root: &Path) -> Probe {
    register_at(root, Point::Preserve)
}

pub(super) fn register_rename_capture(root: &Path) -> Probe {
    register_at(root, Point::RenameCapture)
}

pub(super) fn register_receipt_check(root: &Path) -> Probe {
    register_at(root, Point::ReceiptCheck)
}

fn register_at(root: &Path, point: Point) -> Probe {
    let root = std::fs::canonicalize(root).unwrap();
    let gate = Arc::new(Gate {
        entered: tokio::sync::Notify::new(),
        released: Mutex::new(false),
        wake: Condvar::new(),
    });
    GATES
        .lock()
        .unwrap()
        .insert((root, point), Arc::downgrade(&gate));
    Probe(gate)
}

pub(in crate::service::computer_ws) fn after_preserve(root: &Path) -> AppResult<()> {
    wait_at(root, Point::Preserve)
}

pub(super) fn after_rename_capture(root: &Path) -> AppResult<()> {
    wait_at(root, Point::RenameCapture)
}

pub(super) fn after_receipt_check(root: &Path) -> AppResult<()> {
    wait_at(root, Point::ReceiptCheck)
}

fn wait_at(root: &Path, point: Point) -> AppResult<()> {
    let root = std::fs::canonicalize(root)?;
    let gate = GATES
        .lock()
        .unwrap()
        .remove(&(root, point))
        .and_then(|gate| gate.upgrade());
    if let Some(gate) = gate {
        gate.entered.notify_one();
        let released = gate.released.lock().unwrap();
        let (released, timeout) = gate
            .wake
            .wait_timeout_while(released, Duration::from_secs(20), |released| !*released)
            .unwrap();
        if timeout.timed_out() && !*released {
            return Err(AppError::system(
                "test preservation gate timed out; receipt retained",
            ));
        }
    }
    Ok(())
}
