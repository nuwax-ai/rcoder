//! Real supervisor/guardian/worker startup: opaque discovery cannot veto a
//! newly authorized worker, while an actual older execution lease still can.
use serde_json::{Value, json};
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const BINARY: &str = env!("CARGO_BIN_EXE_file-server-proxy");

#[path = "native_receipt_recovery/cleanup.rs"]
mod cleanup;

fn command(state: &Path, action: &str) -> Command {
    let mut command = Command::new(BINARY);
    command
        .args([action, "--port", "0", "--no-embed"])
        .env("FILE_SERVER_PROXY_HOST", "127.0.0.1")
        .env("FILE_SERVER_PROXY_STATE_DIR", state.parent().unwrap())
        .env_remove(runtime_supervisor::WORKER_ENV)
        .env_remove(runtime_supervisor::TOKEN_ENV)
        .env_remove("FILE_SERVER_PROXY_TS_ENTRY")
        .env_remove("FILE_SERVER_PROXY_LAUNCH_ID");
    command
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

struct Proxy {
    child: Child,
    state: PathBuf,
    log: PathBuf,
    retention: cleanup::Retention,
}

impl Proxy {
    fn start(state: &Path, log: &Path, fixture: &cleanup::Fixture) -> Self {
        std::fs::create_dir_all(state).unwrap();
        let output = File::create(log).unwrap();
        let child = command(state, "start")
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .spawn()
            .unwrap();
        Self {
            child,
            state: state.to_owned(),
            log: log.to_owned(),
            retention: fixture.retention(),
        }
    }

    fn ready(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let owner = read_json(&self.state.join("owner.json"));
            let discovery = read_json(&self.state.join("supervisor.json"));
            if let (Some(owner), Some(discovery)) = (owner, discovery)
                && owner["phase"] == "Running"
                && discovery["snapshot"]["phase"] == "ready"
                && owner["instance_id"] == discovery["snapshot"]["generation"]
            {
                let id = owner["instance_id"].as_str().unwrap();
                let generation =
                    read_json(&self.state.join("work").join(id).join("generation.json")).unwrap();
                let pid = generation["worker_pid"].as_u64().unwrap() as u32;
                assert_ne!(pid, self.child.id(), "the real guardian spawned a worker");
                assert!(process_utils::process_exists(pid).unwrap());
                assert_eq!(generation["phase"], "Running");
                let status = command(&self.state, "status").output().unwrap();
                assert!(status.status.success());
                let status: Value = serde_json::from_slice(&status.stdout).unwrap();
                assert_eq!(status["phase"], "Running");
                return owner;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "supervisor exited"
            );
            assert!(
                Instant::now() < deadline,
                "real startup did not become ready: {}",
                std::fs::read_to_string(&self.log).unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_some() {
            return;
        }
        let result = command(&self.state, "stop").output().unwrap();
        assert!(
            result.status.success(),
            "exact supervised Stop must complete"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "owned supervisor did not exit");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        cleanup::finish(
            &mut self.child,
            &self.state,
            command(&self.state, "stop"),
            &self.retention,
        );
    }
}

#[test]
fn authorized_workers_preserve_opaque_receipts_after_real_predecessor_exit() {
    for kind in [
        "malformed",
        "older_schema",
        "newer_schema",
        "missing_supervisor",
    ] {
        let directory = cleanup::Fixture::new();
        let state = directory
            .path()
            .join("state")
            .join("native-3132372e302e302e31-0");
        let mut previous = Proxy::start(&state, &directory.path().join("old.log"), &directory);
        let old = previous.ready();
        previous.stop();
        let work = state
            .join("work")
            .join(old["instance_id"].as_str().unwrap());
        assert_eq!(
            read_json(&work.join("generation.json")).unwrap()["phase"],
            "Quiescent"
        );
        std::fs::write(work.join("data-proof.txt"), b"retained old data").unwrap();
        let original = match kind {
            "malformed" => b"{broken owner history".to_vec(),
            "older_schema" => {
                let mut old = old.clone();
                old["version"] = json!(0);
                serde_json::to_vec(&old).unwrap()
            }
            "newer_schema" => {
                let mut old = old.clone();
                old["future_diagnostic"] = json!("preserve this field");
                serde_json::to_vec(&old).unwrap()
            }
            _ => {
                let mut old = old.clone();
                old.as_object_mut().unwrap().remove("supervisor_id");
                old["phase"] = json!("Running");
                serde_json::to_vec(&old).unwrap()
            }
        };
        std::fs::write(state.join("owner.json"), &original).unwrap();
        // An old native directory containing only opaque diagnostics is not
        // evidence of an active writer. Keep its bytes through normal start.
        let opaque = state.join("work").join("legacy-retired-diagnostics");
        std::fs::create_dir(&opaque).unwrap();
        std::fs::write(opaque.join("old-note.json"), b"opaque old note").unwrap();
        let report = state.join("work").join("old-diagnostic.json");
        std::fs::write(&report, b"keep historical report bytes").unwrap();
        let mut recovered = Proxy::start(&state, &directory.path().join("new.log"), &directory);
        let new = recovered.ready();
        assert_ne!(new["instance_id"], old["instance_id"]);
        assert!(std::fs::read_dir(&state).unwrap().any(|entry| {
            let path = entry.unwrap().path();
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("owner.history-")
                && std::fs::read(path).unwrap() == original
        }));
        assert_eq!(
            std::fs::read(work.join("data-proof.txt")).unwrap(),
            b"retained old data"
        );
        assert_eq!(
            std::fs::read(opaque.join("old-note.json")).unwrap(),
            b"opaque old note"
        );
        assert_eq!(
            std::fs::read(&report).unwrap(),
            b"keep historical report bytes"
        );
        recovered.stop();
    }
}

#[test]
fn authorized_worker_cannot_replace_history_while_a_legacy_execution_lease_is_held() {
    let directory = cleanup::Fixture::new();
    let state = directory
        .path()
        .join("state")
        .join("native-3132372e302e302e31-0");
    let legacy = state.join("work").join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&legacy).unwrap();
    let lease = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(legacy.join("generation.lock"))
        .unwrap();
    lease.try_lock().unwrap();
    let original = b"{broken owner must remain protected";
    std::fs::write(state.join("owner.json"), original).unwrap();
    let log = directory.path().join("blocked.log");
    let mut proxy = Proxy::start(&state, &log, &directory);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::read_to_string(&log)
            .unwrap()
            .contains("previous native execution lease remains held")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "actual lease refusal was not observed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(std::fs::read(state.join("owner.json")).unwrap(), original);
    assert!(!std::fs::read_dir(&state).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("owner.history-")
    }));
    drop(lease);
    proxy.stop();
}

#[test]
fn unapproved_contender_cannot_replace_a_live_owners_damaged_receipt() {
    let directory = cleanup::Fixture::new();
    let state = directory
        .path()
        .join("state")
        .join("native-3132372e302e302e31-0");
    let mut owner = Proxy::start(&state, &directory.path().join("owner.log"), &directory);
    let receipt = owner.ready();
    let generation = state
        .join("work")
        .join(receipt["instance_id"].as_str().unwrap())
        .join("generation.json");
    let execution = std::fs::read(&generation).unwrap();
    let broken = b"{active owner discovery is damaged";
    std::fs::write(state.join("owner.json"), broken).unwrap();
    let contender = command(&state, "start").output().unwrap();
    assert!(
        !contender.status.success(),
        "no current worker authority was granted"
    );
    assert_eq!(std::fs::read(&generation).unwrap(), execution);
    assert_eq!(std::fs::read(state.join("owner.json")).unwrap(), broken);
    assert!(!std::fs::read_dir(&state).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("owner.history-")
    }));
    owner.stop();
}
