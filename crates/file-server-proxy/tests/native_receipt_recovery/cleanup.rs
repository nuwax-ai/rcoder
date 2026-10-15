//! Failure-only fixture teardown. It cannot turn a failed recovery into a pass.
use std::{
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) type Retention = Arc<AtomicBool>;

pub(super) struct Fixture {
    directory: Option<tempfile::TempDir>,
    retain: Retention,
}

impl Fixture {
    pub(super) fn new() -> Self {
        Self {
            directory: Some(tempfile::tempdir().unwrap()),
            retain: Arc::new(AtomicBool::new(false)),
        }
    }
    pub(super) fn path(&self) -> &Path {
        self.directory.as_ref().unwrap().path()
    }
    pub(super) fn retention(&self) -> Retention {
        self.retain.clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.retain.load(Ordering::Acquire)
            && let Some(directory) = self.directory.take()
        {
            eprintln!(
                "unconfirmed native fixture scope retained: {}",
                directory.keep().display()
            );
        }
    }
}

fn wait(child: &mut Child, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Err(_) => return false,
            Ok(None) if Instant::now() >= deadline => return false,
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

fn execution_settled(state: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(state.join("work")) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else { return false };
        let Ok(kind) = entry.file_type() else {
            return false;
        };
        if kind.is_file() {
            continue;
        }
        if !kind.is_dir() {
            return false;
        }
        let work = entry.path();
        if work.join("generation.json").is_file() {
            let id = entry.file_name();
            let Some(id) = id.to_str() else { return false };
            if !matches!(
                runtime_supervisor::verify_local_quiescent(state, id),
                Ok(Some(_))
            ) {
                return false;
            }
        }
        if !matches!(process_utils::guardian::is_scope_settled(&work), Ok(true)) {
            return false;
        }
    }
    true
}

pub(super) fn finish(child: &mut Child, state: &Path, mut stop: Command, retain: &Retention) {
    let mut forced = false;
    let mut client_exited = true;
    if matches!(child.try_wait(), Ok(None)) {
        // This controls only the current fixture. The ordinary stop() oracle
        // and its business budgets are unchanged; teardown itself is bounded.
        if let Ok(mut client) = stop.stdout(Stdio::null()).stderr(Stdio::null()).spawn()
            && !wait(&mut client, Duration::from_secs(5))
        {
            drop(client.kill());
            client_exited = wait(&mut client, Duration::from_secs(2));
        }
        if !wait(child, Duration::from_secs(10)) {
            forced = true;
            // Retained Child ownership, never a PID recovered from a receipt.
            drop(child.kill());
        }
    }
    let parent_exited = wait(child, Duration::from_secs(5));
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut settled = false;
    if parent_exited {
        loop {
            if execution_settled(state) {
                settled = true;
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    if !client_exited || !parent_exited || !settled {
        retain.store(true, Ordering::Release);
    }
    if forced || !client_exited || !parent_exited || !settled {
        eprintln!(
            "native fixture cleanup: forced={forced}, client_exited={client_exited}, parent_exited={parent_exited}, execution_settled={settled}, scope={}",
            state.display()
        );
        if !std::thread::panicking() {
            panic!("native fixture required forced or unconfirmed cleanup; recovery is not a pass");
        }
    }
}
