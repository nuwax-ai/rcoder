//! Real CLI, owner, guardian and command fault injection. The routing binary
//! is the explicitly named protocol stand-in used by this integration target;
//! official Pingap image/platform validation is a separate gate.
use super::*;

fn process_column(pid: u32, column: &str) -> String {
    let output = Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", column])
        .output()
        .unwrap();
    assert!(output.status.success(), "observe captured process {pid}");
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn observe_process_column(pid: u32, column: &str) -> Option<String> {
    let output = Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", column])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn captured_live_proxy(
    fixture: &Fixture,
    native: &runtime_supervisor::Snapshot,
) -> (u32, u32, PathBuf, Vec<u8>) {
    let confirmation = fixture
        .source
        .join("pingap-runtime/official-proxy-confirmation.json");
    let resident = fixture.state.join("resident").join(&native.supervisor_id);
    let binary = std::env::var_os("APP_CLI_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_app-cli")))
        .canonicalize()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let candidate = (|| {
                let bytes = std::fs::read(&confirmation).ok()?;
                let record: Value = serde_json::from_slice(&bytes).ok()?;
                let pid = u32::try_from(record["confirmation"]["process_id"].as_u64()?).ok()?;
                if !process_utils::process_exists(pid).ok()? {
                    return None;
                }
                let config = record["config_path"].as_str()?;
                if !observe_process_column(pid, "args=")?.ends_with(&format!(" -c {config}")) {
                    return None;
                }
                let guardian = observe_process_column(pid, "ppid=")?.parse::<u32>().ok()?;
                for entry in std::fs::read_dir(resident.join("guardians")).ok()? {
                    let root = entry.ok()?.path().canonicalize().ok()?;
                    let receipt: Value =
                        serde_json::from_slice(&std::fs::read(root.join("receipt.json")).ok()?)
                            .ok()?;
                    if receipt["phase"] != "Running"
                        || receipt["diagnostic_pid"].as_u64() != Some(u64::from(pid))
                        || receipt["resident_identity"]["application_id"] != fixture.app
                        || receipt["resident_identity"]["owner_instance"] != native.supervisor_id
                    {
                        continue;
                    }
                    let expected = format!(
                        "{} --native-command-guardian {}",
                        binary.display(),
                        root.display()
                    );
                    if observe_process_column(guardian, "args=")? != expected {
                        continue;
                    }
                    let lock = std::fs::File::options()
                        .read(true)
                        .write(true)
                        .open(root.join("guardian.lock"))
                        .ok()?;
                    if !matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
                        continue;
                    }
                    if std::fs::read(&confirmation).ok()? != bytes {
                        return None;
                    }
                    return Some((pid, guardian, root, bytes));
                }
                None
            })();
            if let Some(candidate) = candidate {
                return candidate;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "no actual serving proxy guardian was captured: {}\n{}",
            capture_diagnostics(fixture, native),
            fixture.diagnostics()
        )
    })
}

fn capture_diagnostics(fixture: &Fixture, native: &runtime_supervisor::Snapshot) -> String {
    let path = fixture
        .source
        .join("pingap-runtime/official-proxy-confirmation.json");
    let confirmation = std::fs::read(&path)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            serde_json::from_slice::<Value>(&bytes).map_err(|error| error.to_string())
        });
    let mut diagnostics = format!("confirmation_path={path:?} ");
    if let Ok(record) = &confirmation {
        let pid = record["confirmation"]["process_id"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok());
        diagnostics.push_str(&format!(
            "contract={} config={} process_id={pid:?} ",
            record["contract"], record["config_path"]
        ));
        if let Some(pid) = pid {
            let parent = observe_process_column(pid, "ppid=");
            let guardian = parent
                .as_ref()
                .and_then(|parent| parent.parse::<u32>().ok());
            diagnostics.push_str(&format!(
                "command_args={:?} guardian_pid={guardian:?} guardian_args={:?} ",
                observe_process_column(pid, "args="),
                guardian.and_then(|pid| observe_process_column(pid, "args="))
            ));
        }
    } else {
        diagnostics.push_str(&format!("confirmation_error={:?} ", confirmation.err()));
    }
    if let Ok(entries) = std::fs::read_dir(
        fixture
            .state
            .join("resident")
            .join(&native.supervisor_id)
            .join("guardians"),
    ) {
        for entry in entries {
            let root = entry.unwrap().path();
            let receipt: Value =
                serde_json::from_slice(&std::fs::read(root.join("receipt.json")).unwrap()).unwrap();
            let held = std::fs::File::options()
                .read(true)
                .write(true)
                .open(root.join("guardian.lock"))
                .map(|lock| matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
            diagnostics.push_str(&format!("guardian_root={root:?} phase={} diagnostic_pid={} app={} owner={} held_lock={held:?}; ",
                receipt["phase"],receipt["diagnostic_pid"],receipt["resident_identity"]["application_id"],receipt["resident_identity"]["owner_instance"]));
        }
    }
    diagnostics
}

fn signal(pid: u32, name: &str) {
    assert!(pid > 1, "never signal an unbound process");
    assert!(
        Command::new("kill")
            .args([name, &pid.to_string()])
            .status()
            .unwrap()
            .success(),
        "signal captured process {pid} with {name}"
    );
}

/// Keep test fault injection self-cleaning even when a red-baseline assertion
/// fails. The original invocation is rechecked; this never sweeps by name.
struct CapturedOwnerCleanup {
    pid: u32,
    invocation: String,
    armed: bool,
}

impl CapturedOwnerCleanup {
    fn capture(pid: u32) -> Self {
        Self {
            pid,
            invocation: process_column(pid, "args="),
            armed: true,
        }
    }
}

async fn capture_replacement_owner(
    fixture: &mut Fixture,
    native: &runtime_supervisor::Snapshot,
    expected_invocation: &str,
) -> CapturedOwnerCleanup {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let output = Command::new("ps")
                .args(["-ww", "-axo", "pid=,args="])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "query actual isolated test owner invocation"
            );
            let matching: Vec<u32> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
                    let separator = line.find(char::is_whitespace)?;
                    (line[separator..].trim() == expected_invocation)
                        .then(|| line[..separator].parse().ok())
                        .flatten()
                })
                .collect();
            if matching.len() == 1 && process_utils::process_exists(matching[0]).unwrap() {
                let before = runtime_supervisor::control_verified(
                    &fixture.state,
                    runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                    &native.supervisor_id,
                )
                .await
                .unwrap();
                assert_eq!(before.binding.resource, fixture.source);
                assert!(
                    runtime_supervisor::Owner::try_acquire(&fixture.state)
                        .unwrap()
                        .is_none()
                );
                let captured = CapturedOwnerCleanup::capture(matching[0]);
                let after = runtime_supervisor::control_verified(
                    &fixture.state,
                    runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                    &native.supervisor_id,
                )
                .await
                .unwrap();
                assert_eq!(after.supervisor_id, before.supervisor_id);
                assert_eq!(captured.invocation, expected_invocation);
                fixture.owner_pid = Some(captured.pid);
                return captured;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "new owner was not physically bound to captured native authority: {}",
            fixture.diagnostics()
        )
    })
}

impl Drop for CapturedOwnerCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let observed = Command::new("ps")
            .args(["-ww", "-p", &self.pid.to_string(), "-o", "args="])
            .output();
        if observed.is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == self.invocation
        }) {
            let _ = Command::new("kill")
                .args(["-KILL", &self.pid.to_string()])
                .status();
        }
    }
}

async fn released_lock(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let file = std::fs::File::options()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            match file.try_lock() {
                Ok(()) => return,
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(error) => panic!("inspect retained lock {}: {error}", path.display()),
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("captured process did not release its original OS lock");
}

async fn run_succeeded(fixture: &mut Fixture, index: usize) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(status) = fixture.clients[index].try_wait().unwrap() {
                assert!(
                    status.success(),
                    "native_snapshot={:?}\n{}",
                    runtime_supervisor::last_snapshot(&fixture.state),
                    fixture.diagnostics()
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("original run did not converge: {}", fixture.diagnostics()));
}

fn spawn_management(fixture: &mut Fixture, log: &str) -> usize {
    let child = fixture
        .command("serve", log)
        .args(["--admin-addr", "127.0.0.1:0", "--log-dir"])
        .arg(fixture.source.join("logs"))
        .arg("--pingap-bin")
        .arg(&fixture.pingap)
        .env("APP_CLI_CONTROL_ONLY", "true")
        .env(
            "APP_CLI_MANAGEMENT_BOOT_ID",
            format!("boot-{}", fixture.app),
        )
        .spawn()
        .unwrap();
    fixture.clients.push(child);
    fixture.clients.len() - 1
}

#[tokio::test]
async fn control_only_boot_does_not_revoke_later_source_recovery() {
    let mut fixture = Fixture::new();
    assert!(
        fixture
            .command("gen-lock", "control-bootstrap-lock")
            .status()
            .unwrap()
            .success()
    );
    let first_owner = spawn_management(&mut fixture, "control-bootstrap-first");
    let client = client();
    let (_base, _identity, native) = fixture.ready(&client).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = runtime_supervisor::control_verified(
                &fixture.state,
                runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                &native.supervisor_id,
            )
            .await
            .unwrap();
            if status.phase == runtime_supervisor::Phase::Ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap();
    let release = app_cli::manifest::read_release_lock(&fixture.source).unwrap();
    assert!(
        client
            .get(format!("http://127.0.0.1:{}/", release.services[0].port))
            .send()
            .await
            .is_err(),
        "query bootstrap must not start the old business"
    );
    // Consuming a management boot ID alone must not permit an old Running
    // history to resume on a second owner: no Source was accepted yet.
    fixture.clients[first_owner].kill().unwrap();
    fixture.clients[first_owner].wait().unwrap();
    released_lock(&fixture.state.join("owner.lock")).await;
    let first_owner = spawn_management(&mut fixture, "control-bootstrap-still-idle");
    let (base, identity, native) = fixture.ready(&client).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = client
                .get(format!("{base}/v1/deploy/status"))
                .send()
                .await
                .unwrap();
            if response.status().is_success() {
                let current = runtime_supervisor::control_verified(
                    &fixture.state,
                    runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                    &native.supervisor_id,
                )
                .await
                .unwrap();
                if current.phase == runtime_supervisor::Phase::Ready {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        client
            .get(format!("http://127.0.0.1:{}/", release.services[0].port))
            .send()
            .await
            .is_err(),
        "same consumed boot without new Source must remain idle"
    );
    fixture.operation(&client, &base, &identity, "start").await;
    fixture.http(&client, "first").await;
    let desired: Value =
        serde_json::from_slice(&std::fs::read(fixture.state.join("desired.json")).unwrap())
            .unwrap();
    assert_eq!(desired["desired"], "running");
    assert_eq!(
        desired["management_boot_id"],
        format!("boot-{}", fixture.app)
    );
    // The test retains the actual serve Child handle, so fault injection does
    // not rely on a historical PID or process-name sweep.
    fixture.clients[first_owner].kill().unwrap();
    fixture.clients[first_owner].wait().unwrap();
    released_lock(&fixture.state.join("owner.lock")).await;
    let recovered_owner = spawn_management(&mut fixture, "control-bootstrap-recovered");
    let (new_base, new_identity, new_native) = fixture.ready(&client).await;
    fixture.owner_pid = Some(fixture.clients[recovered_owner].id());
    assert_ne!(new_native.supervisor_id, native.supervisor_id);
    fixture.http(&client, "first").await;
    fixture
        .same_owner(&client, &new_base, &new_identity, &new_native)
        .await;
    fixture.manifests("control-bootstrap-new-source");
    let next = fixture.spawn_run("control-bootstrap-run");
    run_succeeded(&mut fixture, next).await;
    fixture.http(&client, "control-bootstrap-new-source").await;
    fixture
        .same_owner(&client, &new_base, &new_identity, &new_native)
        .await;
}

#[tokio::test]
async fn run_after_owner_and_guardian_kill_settles_dead_resident_and_rebuilds_source() {
    let mut fixture = Fixture::new();
    assert!(
        fixture
            .command("gen-lock", "dead-resident-gen-lock")
            .status()
            .unwrap()
            .success()
    );
    let first = fixture.spawn_run("dead-resident-first");
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    fixture.http(&client, "first").await;
    run_succeeded(&mut fixture, first).await;
    fixture.same_owner(&client, &base, &identity, &native).await;
    let owner_pid = fixture.owner_pid.unwrap();
    let (command_pid, guardian_pid, guardian_root, confirmed_bytes) =
        captured_live_proxy(&fixture, &native).await;
    // Freeze only the captured owner so its watchdog cannot repair the injected
    // guardian failure before we crash that same owner.
    let mut crash_cleanup = CapturedOwnerCleanup::capture(owner_pid);
    let owner_invocation = crash_cleanup.invocation.clone();
    signal(owner_pid, "-STOP");
    assert_eq!(
        std::fs::read(
            fixture
                .source
                .join("pingap-runtime/official-proxy-confirmation.json")
        )
        .unwrap(),
        confirmed_bytes
    );
    assert!(process_utils::process_exists(command_pid).unwrap());
    let listener = pingap_config::PingapConfig::new(
        &std::fs::read(fixture.source.join("protocol-listener.toml")).unwrap(),
        true,
    )
    .unwrap();
    let entry = listener.servers["app"]
        .addr
        .split(',')
        .next_back()
        .unwrap()
        .trim();
    assert_eq!(
        client
            .get(format!("http://{entry}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "first"
    );
    signal(guardian_pid, "-KILL");
    assert!(process_utils::kill_process_group_with_fallback(
        command_pid,
        process_utils::KillSignal::SIGKILL
    ));
    signal(owner_pid, "-KILL");
    crash_cleanup.armed = false;
    released_lock(&fixture.state.join("owner.lock")).await;
    released_lock(&guardian_root.join("guardian.lock")).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while process_utils::process_exists(command_pid).unwrap()
            || process_utils::process_group_exists(command_pid).unwrap()
        {
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .expect("actual proxy root and descendants must exit before replacement");
    let receipt_before = std::fs::read(guardian_root.join("receipt.json")).unwrap();
    let interrupted: Value = serde_json::from_slice(&receipt_before).unwrap();
    assert_eq!(interrupted["phase"], "Running");
    assert!(interrupted["root_status"].is_null());
    let lock_metadata = std::fs::metadata(fixture.state.join("owner.lock")).unwrap();
    use std::os::unix::fs::MetadataExt;
    let original_lock_inode = lock_metadata.ino();
    std::fs::write(fixture.source.join("sentinel"), b"preserve workspace").unwrap();
    fixture.manifests("after-dead-resident");
    let second = fixture.spawn_run("dead-resident-second");
    let (new_base, new_identity, new_native) = fixture.ready(&client).await;
    let mut failed_replacement_cleanup =
        capture_replacement_owner(&mut fixture, &new_native, &owner_invocation).await;
    run_succeeded(&mut fixture, second).await;
    fixture.http(&client, "after-dead-resident").await;
    assert_ne!(new_native.supervisor_id, native.supervisor_id);
    let recovered: Value =
        serde_json::from_slice(&std::fs::read(guardian_root.join("receipt.json")).unwrap())
            .unwrap();
    assert_eq!(recovered["phase"], "Quiescent");
    assert!(
        recovered["root_status"].is_null(),
        "cleanup is not a successful old operation"
    );
    assert_eq!(
        std::fs::metadata(fixture.state.join("owner.lock"))
            .unwrap()
            .ino(),
        original_lock_inode,
        "recovery must retain the same lock file"
    );
    assert_eq!(
        std::fs::read(fixture.source.join("sentinel")).unwrap(),
        b"preserve workspace"
    );
    fixture
        .same_owner(&client, &new_base, &new_identity, &new_native)
        .await;
    // A further new-source request converges on that same recovered owner.
    fixture.manifests("after-repeated-run");
    let third = fixture.spawn_run("dead-resident-third");
    run_succeeded(&mut fixture, third).await;
    fixture.http(&client, "after-repeated-run").await;
    fixture
        .same_owner(&client, &new_base, &new_identity, &new_native)
        .await;
    // The ordinary fixture Shutdown cleans the successful path. A red baseline
    // cannot complete recovery, so its exact captured owner is crash-cleaned.
    failed_replacement_cleanup.armed = false;
}
