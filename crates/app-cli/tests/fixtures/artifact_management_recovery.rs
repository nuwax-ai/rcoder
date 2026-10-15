//! Real native owner/guardian/service recovery. Dedicated red/green invocations
//! select the actual official Pingap with APP_CLI_TEST_PINGAP_BIN; the default
//! integration suite retains its explicitly documented protocol fixture.
use super::*;
use sha2::Digest as _;
use std::io::Write as _;

fn observed_args(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "args="])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
fn parent_pid(pid: u32) -> u32 {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "ppid="])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}
fn signal(pid: u32, flag: &str) {
    assert!(pid > 1);
    assert!(
        Command::new("kill")
            .args([flag, &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
}
struct CapturedGuardian {
    pid: u32,
    args: String,
    root: PathBuf,
    command: u32,
    command_args: String,
}

struct GroupKeeper(Child);
impl GroupKeeper {
    fn stop(&mut self) -> std::io::Result<()> {
        if self.0.try_wait()?.is_none() {
            self.0.kill()?;
        }
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while self.0.try_wait()?.is_none() {
            if std::time::Instant::now() >= until {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "exact test keeper did not exit",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}
impl Drop for GroupKeeper {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("dispose exact isolated keeper: {error}");
        }
    }
}
fn process_field(pid: u32, field: &str) -> String {
    let value = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", field])
        .output()
        .unwrap();
    assert!(value.status.success());
    String::from_utf8(value.stdout).unwrap().trim().to_owned()
}
fn keep_guardian_group_non_orphaned(guardian: &CapturedGuardian) -> GroupKeeper {
    use std::io::BufRead as _;
    use std::os::unix::process::CommandExt as _;
    let original_group = Command::new("ps")
        .args(["-p", &guardian.pid.to_string(), "-o", "pgid="])
        .output()
        .unwrap();
    assert!(original_group.status.success());
    let group: u32 = String::from_utf8(original_group.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        group, guardian.pid,
        "captured guardian must own its actual independent group"
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_tree-fixture"));
    command
        .args(["hold", "0"])
        .stdout(Stdio::piped())
        .process_group(i32::try_from(group).unwrap());
    let mut keeper = GroupKeeper(command.spawn().unwrap());
    let mut ready = String::new();
    std::io::BufReader::new(keeper.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert!(
        ready.starts_with("READY "),
        "isolated group keeper startup: {ready}"
    );
    assert_eq!(parent_pid(keeper.0.id()), std::process::id());
    assert_eq!(
        process_field(guardian.pid, "sess="),
        process_field(keeper.0.id(), "sess=")
    );
    assert_eq!(
        process_field(guardian.pid, "sess="),
        process_field(std::process::id(), "sess=")
    );
    assert_ne!(
        process_field(std::process::id(), "pgid="),
        group.to_string()
    );
    let actual_group = Command::new("ps")
        .args(["-p", &keeper.0.id().to_string(), "-o", "pgid="])
        .output()
        .unwrap();
    assert!(actual_group.status.success());
    assert_eq!(
        String::from_utf8(actual_group.stdout).unwrap().trim(),
        group.to_string()
    );
    eprintln!(
        "retained real group keeper pid={} parent={} guardian_pgid={group}",
        keeper.0.id(),
        std::process::id()
    );
    keeper
}
struct FrozenGuardians(Vec<CapturedGuardian>);
impl FrozenGuardians {
    fn release(&mut self) {
        for captured in &self.0 {
            let actual = observed_args(captured.pid);
            eprintln!(
                "release captured guardian pid={} expected={:?} actual={actual:?} command={} root={}",
                captured.pid,
                captured.args,
                captured.command,
                captured.root.display()
            );
            if actual.as_deref() == Some(captured.args.as_str()) {
                signal(captured.pid, "-CONT");
            }
        }
    }
}
impl Drop for FrozenGuardians {
    fn drop(&mut self) {
        self.release();
        // Failed-test disposal is separate from the asserted native cleanup.
        // Only previously captured exact unique command argv may be signalled;
        // a passing test already proved these old commands exited normally.
        for captured in &self.0 {
            if observed_args(captured.command).as_deref() == Some(captured.command_args.as_str()) {
                eprintln!(
                    "dispose failed isolated command {} {:?}",
                    captured.command, captured.command_args
                );
                signal(captured.command, "-KILL");
            }
        }
    }
}
async fn capture_guardians(
    fixture: &Fixture,
    native: &runtime_supervisor::Snapshot,
) -> Vec<CapturedGuardian> {
    let executable = std::env::var_os("APP_CLI_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_app-cli").into())
        .canonicalize()
        .unwrap();
    let scopes = [
        fixture
            .state
            .join("work")
            .join(native.generation.as_ref().unwrap()),
        fixture.state.join("resident").join(&native.supervisor_id),
    ]
    .map(|path| path.canonicalize().unwrap());
    let until = tokio::time::Instant::now() + Duration::from_secs(9);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut result = Vec::new();
            let mut diagnostic = Vec::new();
            for scope in &scopes {
                let Ok(entries) = std::fs::read_dir(scope.join("guardians")) else {
                    continue;
                };
                for entry in entries {
                    let root = entry.unwrap().path().canonicalize().unwrap();
                    let Ok(raw) = std::fs::read(root.join("receipt.json")) else {
                        continue;
                    };
                    let row: Value = serde_json::from_slice(&raw).unwrap();
                    diagnostic.push(format!("root={} phase={} pid={} root_status={} instance={}",root.display(),row["phase"],row["diagnostic_pid"],row["root_status"],row["instance_id"]));
                    if row["phase"] != "Running" || !row["root_status"].is_null() {
                        continue;
                    }
                    let Some(command) = row["diagnostic_pid"]
                        .as_u64()
                        .and_then(|v| u32::try_from(v).ok())
                    else {
                        continue;
                    };
                    if !process_utils::process_exists(command).unwrap() {
                        continue;
                    }
                    let guardian = parent_pid(command);
                    let expected = format!(
                        "{} --native-command-guardian {}",
                        executable.display(),
                        root.display()
                    );
                    diagnostic.push(format!("command={command} guardian={guardian} expected={expected:?} actual={:?}",observed_args(guardian)));
                    if observed_args(guardian).as_deref() != Some(expected.as_str()) {
                        continue;
                    }
                    let lock = std::fs::File::options()
                        .read(true)
                        .write(true)
                        .open(root.join("guardian.lock"))
                        .unwrap();
                    if !matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)) {
                        continue;
                    }
                    result.push(CapturedGuardian {
                        pid: guardian,
                        args: expected,
                        root,
                        command,
                        command_args: observed_args(command).unwrap(),
                    });
                }
            }
            if result.iter().any(|r| r.root.starts_with(&scopes[0]))
                && result.iter().any(|r| r.root.starts_with(&scopes[1]))
            {
                return result;
            }
            if tokio::time::Instant::now() >= until { panic!("guardian capture whitelist native={native:?} scopes={scopes:?} diagnostics={diagnostic:?}"); }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "actual business/proxy guardians not captured: {error}; {}",
            fixture.diagnostics()
        )
    })
}
fn spawn_owner(fixture: &mut Fixture, label: &str) -> usize {
    let child = fixture
        .command("serve", label)
        .args(["--control-only", "--admin-addr", "127.0.0.1:0", "--log-dir"])
        .arg(fixture.source.join("logs"))
        .arg("--pingap-bin")
        .arg(&fixture.pingap)
        .env(
            "APP_CLI_MANAGEMENT_BOOT_ID",
            format!("artifact-boot-{}", fixture.app),
        )
        .spawn()
        .unwrap();
    fixture.clients.push(child);
    fixture.clients.len() - 1
}
async fn management_ready(fixture: &Fixture, native: &runtime_supervisor::Snapshot) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let actual = runtime_supervisor::control_verified(
                &fixture.state,
                runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                &native.supervisor_id,
            )
            .await
            .unwrap();
            if actual.phase == runtime_supervisor::Phase::Ready {
                return;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
}
fn archive(fixture: &Fixture, marker: &str) -> (String, Vec<u8>) {
    let mut release = app_cli::manifest::read_release_lock(&fixture.source).unwrap();
    release.release_id = format!("artifact-built-{}", uuid::Uuid::new_v4().simple());
    release.services[0].devrun = None;
    release.services[0].run.command = vec![
        "sh".into(),
        "-ec".into(),
        "printf 'started\\n' >> \"$1\"; exec \"$2\" http \"$3\"".into(),
        "owned-artifact".into(),
        fixture
            .source
            .join("artifact-starts")
            .to_string_lossy()
            .into_owned(),
        env!("CARGO_BIN_EXE_tree-fixture").into(),
        marker.into(),
    ];
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, content) in [
        (
            "release.lock.toml",
            toml::to_string_pretty(&release).unwrap(),
        ),
        (
            "workspace.manifest.toml",
            std::fs::read_to_string(fixture.source.join("workspace.manifest.toml")).unwrap(),
        ),
        (
            "web/project.manifest.toml",
            std::fs::read_to_string(fixture.source.join("web/project.manifest.toml")).unwrap(),
        ),
        (
            "protocol-listener.toml",
            std::fs::read_to_string(fixture.source.join("protocol-listener.toml")).unwrap(),
        ),
    ] {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    (release.release_id, zip.finish().unwrap().into_inner())
}
fn activation_diagnostics(fixture: &Fixture) -> String {
    let previous = fixture.source.parent().unwrap().join(".previous");
    let mut output = fixture.diagnostics();
    output.push_str(&format!(
        "\ncontrol binary={} exists={} previous_control_binary={} exists={}\n",
        fixture.pingap.display(),
        fixture.pingap.is_file(),
        previous.join("protocol-pingap").display(),
        previous.join("protocol-pingap").is_file()
    ));
    for root in [fixture.source.clone(), previous] {
        for directory in [root.clone(), root.join("logs")] {
            for entry in std::fs::read_dir(&directory)
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                if path.is_file()
                    && (matches!(
                        path.extension().and_then(|x| x.to_str()),
                        Some("stderr" | "stdout")
                    ) || name.starts_with("owner-bootstrap-"))
                    && let Ok(content) = std::fs::read_to_string(&path)
                {
                    let tail = content
                        .lines()
                        .rev()
                        .take(100)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("\n");
                    output.push_str(&format!("\n{}:\n{tail}\n", path.display()));
                }
            }
        }
    }
    output
}
async fn operation_response(
    fixture: &Fixture,
    client: &reqwest::Client,
    base: &str,
    id: &str,
) -> (u16, Value) {
    let token = std::fs::read_to_string(fixture.state.join("token")).unwrap();
    let response = client
        .get(format!("{base}/v1/runtime/operations/{id}"))
        .header("X-Deploy-Token", token.trim())
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json::<Value>().await.unwrap())
}
async fn operation(fixture: &Fixture, client: &reqwest::Client, base: &str, id: &str) -> Value {
    operation_response(fixture, client, base, id).await.1["data"].clone()
}

#[derive(Clone, Copy)]
enum Following {
    Publish,
    Cancel,
    NativeStop,
}
async fn scenario(url_artifact: bool, following: Following) {
    let mut fixture = Fixture::new();
    // Control binaries are retained outside the code directory replaced by a
    // URL deployment, just like the official runtime binary. The publication
    // runtime remains inside code so the real activation-path regression stays
    // covered; this moves only the portable test executable.
    fixture.pingap = protocol_support::write_pingap(fixture._directory.path());
    if let Some(binary) = std::env::var_os("APP_CLI_TEST_PINGAP_BIN") {
        fixture.pingap = PathBuf::from(binary).canonicalize().unwrap();
        assert!(
            Command::new(&fixture.pingap)
                .arg("--version")
                .status()
                .unwrap()
                .success()
        );
    }
    assert!(
        fixture
            .command("gen-lock", "artifact-recovery-lock")
            .status()
            .unwrap()
            .success()
    );
    let first_marker = format!("first-{}", fixture.app);
    fixture.manifests(&first_marker);
    let first = spawn_owner(&mut fixture, "artifact-recovery-original-owner");
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    management_ready(&fixture, &native).await;
    fixture.operation(&client, &base, &identity, "start").await;
    fixture.http(&client, &first_marker).await;
    let native = runtime_supervisor::control_verified(
        &fixture.state,
        runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        &native.supervisor_id,
    )
    .await
    .unwrap();
    let (_built, bytes) = archive(&fixture, "artifact-after-cleanup");
    let artifact_id = format!("owned-{}", uuid::Uuid::new_v4().simple());
    std::fs::create_dir_all(fixture.source.join("builds")).unwrap();
    std::fs::write(
        fixture
            .source
            .join("builds")
            .join(format!("workspace-package-{artifact_id}.zip")),
        &bytes,
    )
    .unwrap();
    let guardians = capture_guardians(&fixture, &native).await;
    let roots: Vec<_> = guardians.iter().map(|g| g.root.clone()).collect();
    let commands: Vec<_> = guardians.iter().map(|g| g.command).collect();
    let owner = fixture.clients[first].id();
    // POSIX sends SIGHUP+SIGCONT to an orphaned stopped group. Keep each
    // original guardian group non-orphaned with a retained test Child; this
    // preserves the intended delayed-cleanup fault, rather than killing the
    // only actual tree authority when its owner dies.
    let keepers: Vec<_> = guardians
        .iter()
        .map(keep_guardian_group_non_orphaned)
        .collect();
    signal(owner, "-STOP");
    for g in &guardians {
        assert_eq!(observed_args(g.pid).as_deref(), Some(g.args.as_str()));
        signal(g.pid, "-STOP");
    }
    let mut frozen = FrozenGuardians(guardians);
    fixture.clients[first].kill().unwrap();
    fixture.clients[first].wait().unwrap();
    assert!(
        commands
            .iter()
            .all(|pid| process_utils::process_exists(*pid).unwrap()),
        "old real service/proxy must remain alive during the injected delayed cleanup"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if runtime_supervisor::Owner::try_acquire(&fixture.state)
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let replacement = spawn_owner(&mut fixture, "artifact-recovery-replacement-owner");
    let (base, identity, newnative) = fixture.ready(&client).await;
    fixture.owner_pid = Some(fixture.clients[replacement].id());
    let pending = runtime_supervisor::control_verified(
        &fixture.state,
        runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        &newnative.supervisor_id,
    )
    .await
    .unwrap();
    assert_eq!(
        pending.phase,
        runtime_supervisor::Phase::CleanupPending,
        "real cleanup premise: {pending:?}"
    );
    let downloads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = downloads.clone();
    let served = bytes.clone();
    let asset = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/artifact.zip", asset.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let router = axum::Router::new().route(
            "/artifact.zip",
            axum::routing::get(move || {
                let value = served.clone();
                let count = observed.clone();
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    value
                }
            }),
        );
        axum::serve(asset, router).await.unwrap();
    });
    let id = format!("artifact-recovery-{}", uuid::Uuid::new_v4().simple());
    let token = std::fs::read_to_string(fixture.state.join("token")).unwrap();
    let status: Value = client
        .get(format!("{base}/v1/runtime/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let input = if url_artifact {
        json!({"source":"url","value":{"url":url,"sha256":hex::encode(sha2::Sha256::digest(&bytes))}})
    } else {
        json!({"source":"artifact_id","value":{"artifact_id":artifact_id}})
    };
    let response=client.post(format!("{base}/v1/runtime/operations")).header("X-Deploy-Token",token.trim()).json(&json!({"operation_id":id,"kind":"deploy","expected_runtime_instance_id":identity["runtime_instance_id"],"expected_revision":status["data"]["revision"],"workspace_id":identity["workspace_id"],"profile":{"profile":"artifact","input":{"artifact":input}}})).send().await.unwrap();
    assert_eq!(
        response.status(),
        reqwest::StatusCode::ACCEPTED,
        "{}",
        response.text().await.unwrap()
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let waiting = operation(&fixture, &client, &base, &id).await;
    assert_eq!(waiting["operation_id"], id);
    assert!(
        waiting["state"] == "accepted" || waiting["state"] == "running",
        "original Artifact must wait for actual old cleanup, not fail: {waiting}; native={:?}; {}",
        runtime_supervisor::last_snapshot(&fixture.state),
        fixture.diagnostics()
    );
    assert!(!fixture.source.join("artifact-starts").exists());
    assert_eq!(downloads.load(std::sync::atomic::Ordering::SeqCst), 0);
    match following {
        Following::Publish => {}
        Following::Cancel => {
            let response = client
                .post(format!("{base}/v1/runtime/operations/{id}/cancel"))
                .header("X-Deploy-Token", token.trim())
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
        }
        Following::NativeStop => {
            let mut stop = runtime_supervisor::Request::new(runtime_supervisor::Action::StopWork);
            stop.capture_generation(pending.generation.as_deref());
            let observed = runtime_supervisor::control_verified(
                &fixture.state,
                stop,
                &newnative.supervisor_id,
            )
            .await
            .unwrap();
            assert_eq!(observed.intent, runtime_supervisor::Intent::Stopped);
        }
    }
    frozen.release();
    let mut latest_response = None;
    let result = tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let (status, envelope) = operation_response(&fixture, &client, &base, &id).await;
            let row = envelope["data"].clone();
            latest_response = Some((status, envelope));
            if row["state"] == "succeeded"
                || row["state"] == "failed"
                || row["state"] == "cancelled"
                || row["state"] == "recovery_required"
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "original Artifact did not converge: {error}; id={id}; latest_http={latest_response:?}; native={:?}; {}",
            runtime_supervisor::last_snapshot(&fixture.state),
            activation_diagnostics(&fixture)
        )
    });
    match following {
        Following::Publish => {
            assert_eq!(
                result["state"],
                "succeeded",
                "same original Artifact must finish: {result}; {}",
                fixture.diagnostics()
            );
            assert_eq!(result["operation_id"], id);
            assert_eq!(
                result["runtime_instance_id"],
                identity["runtime_instance_id"]
            );
            let execution = if url_artifact {
                fixture.source.clone()
            } else {
                fixture.source.join(".run")
            };
            let release = app_cli::manifest::read_release_lock(&execution).unwrap();
            let port = release.services[0].port;
            assert_eq!(
                client
                    .get(format!("http://127.0.0.1:{port}/"))
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap(),
                "artifact-after-cleanup"
            );
            assert_eq!(
                std::fs::read_to_string(fixture.source.join("artifact-starts"))
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
        }
        Following::Cancel | Following::NativeStop => {
            if matches!(following, Following::Cancel) {
                assert_eq!(result["state"], "cancelled", "{result}");
            } else {
                assert!(
                    result["state"] == "failed" || result["state"] == "cancelled",
                    "old Artifact must not override accepted native Stop: {result}"
                );
                assert_eq!(
                    runtime_supervisor::last_snapshot(&fixture.state)
                        .unwrap()
                        .intent,
                    runtime_supervisor::Intent::Stopped
                );
            }
            assert_eq!(result["operation_id"], id);
            tokio::time::sleep(Duration::from_millis(700)).await;
            assert!(
                !fixture.source.join("artifact-starts").exists(),
                "cancelled/stopped Artifact must not start late"
            );
            assert_eq!(downloads.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let all = roots.iter().all(|root| {
                let row: Value =
                    serde_json::from_slice(&std::fs::read(root.join("receipt.json")).unwrap())
                        .unwrap();
                row["phase"] == "Quiescent"
            }) && commands
                .iter()
                .all(|pid| !process_utils::process_exists(*pid).unwrap());
            if all {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    for root in roots {
        let row: Value =
            serde_json::from_slice(&std::fs::read(root.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(row["phase"], "Quiescent");
        let lock = std::fs::File::options()
            .read(true)
            .write(true)
            .open(root.join("guardian.lock"))
            .unwrap();
        assert!(lock.try_lock().is_ok());
    }
    assert!(
        commands
            .iter()
            .all(|pid| !process_utils::process_exists(*pid).unwrap())
    );
    server.abort();
    for mut keeper in keepers {
        keeper.stop().unwrap();
    }
}
#[tokio::test]
async fn local_artifact_waits_for_actual_old_owner_cleanup_then_original_id_serves_http() {
    scenario(false, Following::Publish).await;
}
#[tokio::test]
async fn url_artifact_waits_for_actual_old_owner_cleanup_then_original_id_serves_http() {
    scenario(true, Following::Publish).await;
}

#[tokio::test]
async fn cancelled_artifact_waiting_for_old_cleanup_never_downloads_or_starts_late() {
    scenario(true, Following::Cancel).await;
}
#[tokio::test]
async fn native_stop_during_artifact_old_cleanup_preserves_stopped_intent_and_no_late_start() {
    scenario(false, Following::NativeStop).await;
}
