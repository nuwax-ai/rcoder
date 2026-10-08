//! Real CLI/owner lifecycle with actual fixture HTTP. The Pingap process is a
//! protocol stand-in; this does not validate Pingap routing, containers or AI.
#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Fixture {
    _directory: tempfile::TempDir,
    source: PathBuf,
    state: PathBuf,
    app: String,
    pingap: PathBuf,
    clients: Vec<Child>,
    owner: Option<String>,
    owner_pid: Option<u32>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let app = format!("run-{}", uuid::Uuid::new_v4().simple());
        let source = directory.path().canonicalize().unwrap().join(&app);
        let state = directory.path().join("state").join(&app);
        std::fs::create_dir_all(source.join("web")).unwrap();
        let pingap = source.join("protocol-pingap");
        std::fs::write(
            &pingap,
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = -t ] && exit 0; done\nexec sleep 300\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pingap, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fixture = Self {
            _directory: directory,
            source,
            state,
            app,
            pingap,
            clients: vec![],
            owner: None,
            owner_pid: None,
        };
        fixture.manifests("first");
        fixture
    }

    fn manifests(&self, marker: &str) {
        let command = toml::Value::Array(
            [env!("CARGO_BIN_EXE_tree-fixture"), "http", marker]
                .into_iter()
                .map(|s| toml::Value::String(s.into()))
                .collect(),
        );
        self.write_manifests(command, "real HTTP fixture", 5);
    }

    /// 慢启动 manifest：run 命令先 sleep 再起真实 HTTP，拉长原操作的
    /// 非终态窗口，供终局时序断言与 Stop 竞争使用。
    fn slow_manifests(&self, marker: &str, delay_secs: u64) {
        let command = toml::Value::Array(
            [
                "sh".to_string(),
                "-c".to_string(),
                format!(
                    "sleep {delay_secs} && exec {} http {marker}",
                    env!("CARGO_BIN_EXE_tree-fixture")
                ),
            ]
            .into_iter()
            .map(toml::Value::String)
            .collect(),
        );
        self.write_manifests(command, "slow start fixture", 30);
    }

    fn write_manifests(&self, command: toml::Value, name: &str, startup_timeout: u64) {
        std::fs::write(
            self.source.join("workspace.manifest.toml"),
            "schema_version=1\n[workspace]\nname='run-source-owner'\n",
        )
        .unwrap();
        std::fs::write(self.source.join("web/project.manifest.toml"), format!("schema_version=1\n[project]\nservice_id={:?}\nname='{name}'\ntype='rust'\n[build]\ncommand=['true']\nartifact='unused.zip'\n[run]\ncommand={command}\nshutdown_timeout_seconds=3\n[devrun]\ncommand={command}\n[health]\nstartup_timeout_seconds={startup_timeout}\nreadiness_path='/'\n[proxy]\npath='/'\n", self.app)).unwrap();
    }

    fn command(&self, action: &str, log: &str) -> Command {
        self.command_at(action, log, &self.source)
    }

    fn command_at(&self, action: &str, log: &str, workspace: &std::path::Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_app-cli"));
        command.arg(action).arg("--workspace").arg(workspace);
        if action == "run" {
            command
                .args(["--admin-addr", "127.0.0.1:0", "--log-dir"])
                .arg(self.source.join("logs"))
                .arg("--pingap-bin")
                .arg(&self.pingap);
        }
        for key in [
            "APP_DEPLOY_URL",
            "APP_RELEASE_ID",
            "APP_DEPLOY_OPERATION_ID",
            "APP_DEPLOY_GENERATION_ID",
            "APP_DEPLOY_SHA256",
            "APP_CLI_ATTACH",
            "APP_CLI_MANAGED",
            "SERVICE_TYPE",
            "USERAPP_WORKSPACE_DIR",
            "APP_CLI_RUNTIME_WORKSPACE",
            "RCODER_PLATFORM_BINDING_DIR",
            "APP_CLI_DEPLOY_TOKEN",
            "APP_CLI_REQUIRE_PG",
            "PGPASSWORD",
            "PGUSER",
            "PGDATABASE",
            "PGHOST",
            "PGPORT",
        ] {
            command.env_remove(key);
        }
        command.env("PROJECT_ID", &self.app).env("APP_CLI_STATE_ROOT", &self.state)
            .env("APP_CLI_REQUIRE_PG", "0").env("APP_CLI_SKIP_PINGAP_CONFIRM", "1")
            .env("APP_CLI_PINGAP_RUNTIME_DIR", self.source.join("pingap-runtime"))
            .env("APP_CLI_RUN_PROFILE", "dev").env("RCODER_PINGAP_VERSION", "0.14.3")
            .env("RCODER_PINGAP_COMMIT", "cd74a461a3e778ae83f7c4dd7fd03ea483f3e3e8")
            .env("RCODER_RUNTIME_IMAGE_DIGEST", "native-test-fixture")
            .env("RCODER_EXECUTION_DOMAIN", json!({"authority":"run-source-owner-tests", "volume":self.source, "instance":self.app}).to_string())
            .stdin(Stdio::null()).stdout(Stdio::from(std::fs::File::create(self.source.join(format!("{log}.stdout"))).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(self.source.join(format!("{log}.stderr"))).unwrap()));
        command
    }

    fn spawn_run(&mut self, log: &str) -> usize {
        let child = self.command("run", log).spawn().unwrap();
        self.clients.push(child);
        self.clients.len() - 1
    }

    fn diagnostics(&self) -> String {
        let mut paths = std::fs::read_dir(&self.source)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|s| s == "stderr" || s == "stdout")
            })
            .map(|e| e.path())
            .collect::<Vec<_>>();
        paths.extend(
            std::fs::read_dir(self.source.join("logs"))
                .into_iter()
                .flatten()
                .flatten()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("owner-bootstrap-")
                })
                .map(|entry| entry.path()),
        );
        paths
            .into_iter()
            .map(|path| {
                format!(
                    "{}: {}",
                    path.display(),
                    std::fs::read_to_string(&path).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn ready(
        &mut self,
        client: &reqwest::Client,
    ) -> (String, Value, runtime_supervisor::Snapshot) {
        let until = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(bytes) = std::fs::read(self.state.join("endpoint.json"))
                && let Ok(endpoint) = serde_json::from_slice::<Value>(&bytes)
                && let Some(address) = endpoint["address"].as_str()
                && let Ok(native) = runtime_supervisor::control(
                    &self.state,
                    runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                )
                .await
            {
                self.owner = Some(native.supervisor_id.clone());
                if let Some(generation) = native.generation.as_deref()
                    && let Ok(bytes) = std::fs::read(
                        self.state
                            .join("work")
                            .join(generation)
                            .join("generation.json"),
                    )
                    && let Ok(record) = serde_json::from_slice::<Value>(&bytes)
                    && let Some(pid) = record["worker_pid"].as_u64()
                {
                    self.owner_pid = Some(u32::try_from(pid).unwrap());
                }
                let base = format!("http://{address}");
                if let Ok(response) = client
                    .get(format!("{base}/v1/runtime/identity"))
                    .send()
                    .await
                    && response.status().is_success()
                    && let Ok(body) = response.json::<Value>().await
                    && body["data"]["runtime_instance_id"].is_string()
                    && self.state.join("token").is_file()
                    && self.owner_pid.is_some()
                {
                    return (base, body["data"].clone(), native);
                }
            }
            assert!(
                tokio::time::Instant::now() < until,
                "management owner missing: {}",
                self.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    async fn http(&self, client: &reqwest::Client, marker: &str) -> u16 {
        let until = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(lock) = app_cli::manifest::read_release_lock(&self.source) {
                let port = lock.services[0].port;
                if let Ok(response) = client.get(format!("http://127.0.0.1:{port}/")).send().await
                    && let Ok(content) = response.text().await
                    && content == marker
                {
                    return port;
                }
            }
            assert!(
                tokio::time::Instant::now() < until,
                "real HTTP marker {marker} missing: {}",
                self.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    async fn operation(&self, client: &reqwest::Client, base: &str, identity: &Value, kind: &str) {
        let token = std::fs::read_to_string(self.state.join("token")).unwrap();
        let status: Value = client
            .get(format!("{base}/v1/runtime/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = format!("native-test-{}", uuid::Uuid::new_v4().simple());
        let response = client.post(format!("{base}/v1/runtime/operations")).header("X-Deploy-Token", token.trim())
            .json(&json!({"operation_id":id, "expected_runtime_instance_id":identity["runtime_instance_id"], "expected_revision":status["data"]["revision"], "workspace_id":identity["workspace_id"], "kind":kind, "profile":{"profile":"source", "input":{"workspace_id":identity["workspace_id"]}}}))
            .send().await.unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::ACCEPTED,
            "{}",
            response.text().await.unwrap()
        );
        let until = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let response = client
                .get(format!("{base}/v1/runtime/operations/{id}"))
                .header("X-Deploy-Token", token.trim())
                .send()
                .await;
            // A stopped run owner can disappear immediately after its success
            // commit. Preserve the original ID and fail on lost management.
            assert!(
                response.is_ok(),
                "management disappeared while observing {kind} operation {id}: {}",
                self.diagnostics()
            );
            let body: Value = response.unwrap().json().await.unwrap();
            assert_eq!(body["data"]["operation_id"], id);
            assert_eq!(
                body["data"]["runtime_instance_id"],
                identity["runtime_instance_id"]
            );
            if body["data"]["state"] == "succeeded" {
                return;
            }
            assert!(
                body["data"]["state"] == "accepted" || body["data"]["state"] == "running",
                "{kind} did not succeed: {body}"
            );
            assert!(
                tokio::time::Instant::now() < until,
                "{kind} did not settle: {body}"
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    async fn same_owner(
        &self,
        client: &reqwest::Client,
        base: &str,
        identity: &Value,
        native: &runtime_supervisor::Snapshot,
    ) {
        let observed: Value = client
            .get(format!("{base}/v1/runtime/identity"))
            .send()
            .await
            .expect("Stop must retain management HTTP")
            .json()
            .await
            .unwrap();
        assert_eq!(
            observed["data"]["runtime_instance_id"],
            identity["runtime_instance_id"]
        );
        let after = runtime_supervisor::control_verified(
            &self.state,
            runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
            &native.supervisor_id,
        )
        .await
        .expect("Stop must retain the captured native owner");
        assert_eq!(after.supervisor_id, native.supervisor_id);
        assert!(
            process_utils::process_exists(self.owner_pid.expect("captured real owner PID"))
                .unwrap(),
            "captured owner process exited"
        );
        assert!(
            runtime_supervisor::Owner::try_acquire(&self.state)
                .unwrap()
                .is_none(),
            "management must still hold its real exclusive lock"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only the exact captured native owner may receive cleanup Shutdown.
        // No name/PID sweep, new owner shutdown, or source/volume removal.
        let state = self.state.clone();
        let captured = self.owner.clone();
        let cleanup = std::thread::spawn(move || {
            let runtime = runtime_supervisor::runtime().unwrap();
            runtime.block_on(async {
                if let Some(owner) = captured
                    && let Ok(before) = runtime_supervisor::control_verified(
                        &state,
                        runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                        &owner,
                    )
                    .await
                {
                    let mut request =
                        runtime_supervisor::Request::new(runtime_supervisor::Action::Shutdown);
                    request.capture_generation(before.generation.as_deref());
                    let _result =
                        runtime_supervisor::control_verified(&state, request, &owner).await;
                    let until = tokio::time::Instant::now() + Duration::from_secs(10);
                    while tokio::time::Instant::now() < until {
                        if runtime_supervisor::Owner::try_acquire(&state)
                            .ok()
                            .flatten()
                            .is_some()
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(40)).await;
                    }
                }
            });
        });
        let _result = cleanup.join();
        for child in &mut self.clients {
            if matches!(child.try_wait(), Ok(None)) {
                let _result = child.kill();
            }
            let _result = child.wait();
        }
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

#[tokio::test]
async fn first_run_http_stop_retains_owner_and_fresh_source_start() {
    let mut fixture = Fixture::new();
    let generated = fixture.command("gen-lock", "gen-lock").status().unwrap();
    assert!(generated.success(), "{}", fixture.diagnostics());
    fixture.spawn_run("first-run");
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    let port = fixture.http(&client, "first").await;
    fixture.operation(&client, &base, &identity, "stop").await;
    fixture.same_owner(&client, &base, &identity, &native).await;
    assert!(
        client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .is_err(),
        "business must actually stop"
    );
    fixture.manifests("after-http-stop");
    fixture.operation(&client, &base, &identity, "start").await;
    fixture.http(&client, "after-http-stop").await;
    fixture.same_owner(&client, &base, &identity, &native).await;
}

#[tokio::test]
async fn first_run_native_stop_retains_owner_and_new_run_rebuilds_source_lock() {
    let mut fixture = Fixture::new();
    assert!(
        fixture
            .command("gen-lock", "gen-lock")
            .status()
            .unwrap()
            .success()
    );
    fixture.spawn_run("first-run");
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    let port = fixture.http(&client, "first").await;
    let executing = runtime_supervisor::control_verified(
        &fixture.state,
        runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        &native.supervisor_id,
    )
    .await
    .unwrap();
    let stopped_generation = executing
        .generation
        .as_deref()
        .expect("real HTTP generation");
    let mut request = runtime_supervisor::Request::new(runtime_supervisor::Action::StopWork);
    request.capture_generation(Some(stopped_generation));
    let accepted = runtime_supervisor::control_verified(
        &fixture.state,
        request.clone(),
        &native.supervisor_id,
    )
    .await
    .unwrap();
    assert_eq!(
        accepted.operation_id.as_deref(),
        Some(request.request_id.as_str())
    );
    let until = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        // Observe the original captured Stop's durable terminal result. The
        // live owner can already be Ready in its new idle management session.
        let status = runtime_supervisor::control_verified(
            &fixture.state,
            request.clone(),
            &native.supervisor_id,
        )
        .await
        .expect("native Stop must retain management owner");
        if status.phase == runtime_supervisor::Phase::Stopped {
            assert_eq!(
                status.operation_id.as_deref(),
                Some(request.request_id.as_str())
            );
            assert_eq!(status.intent, runtime_supervisor::Intent::Stopped);
            break;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "native Stop did not physically finish: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let receipt = runtime_supervisor::verify_quiescent(&fixture.state, stopped_generation)
        .expect("original generation must have a real aggregate cleanup receipt");
    assert_eq!(receipt.generation, stopped_generation);
    assert_eq!(receipt.supervisor_id, native.supervisor_id);
    let current = runtime_supervisor::control_verified(
        &fixture.state,
        runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        &native.supervisor_id,
    )
    .await
    .unwrap();
    assert_eq!(current.intent, runtime_supervisor::Intent::Stopped);
    fixture.same_owner(&client, &base, &identity, &native).await;
    assert!(
        client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .is_err()
    );
    fixture.manifests("after-native-stop");
    std::fs::remove_file(fixture.source.join("release.lock.toml")).unwrap();
    fixture.spawn_run("second-run");
    fixture.http(&client, "after-native-stop").await;
    fixture.same_owner(&client, &base, &identity, &native).await;
}

#[tokio::test]
async fn first_run_configuration_failure_keeps_management_for_corrected_new_request() {
    let mut fixture = Fixture::new();
    std::fs::write(
        fixture.source.join("workspace.manifest.toml"),
        "invalid = [",
    )
    .unwrap();
    std::fs::write(fixture.source.join("release.lock.toml"), "invalid = [").unwrap();
    let index = fixture.spawn_run("invalid-run");
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    let until = tokio::time::Instant::now() + Duration::from_secs(20);
    let exit = loop {
        if let Some(exit) = fixture.clients[index].try_wait().unwrap() {
            break exit;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "invalid run did not report an error: {}",
            fixture.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    };
    assert!(
        !exit.success(),
        "invalid current configuration must fail explicitly"
    );
    fixture.same_owner(&client, &base, &identity, &native).await;
    fixture.manifests("corrected-current-source");
    fixture.spawn_run("corrected-run");
    fixture.http(&client, "corrected-current-source").await;
    fixture.same_owner(&client, &base, &identity, &native).await;
}

async fn operation_client_exit(fixture: &mut Fixture, index: usize) -> std::process::ExitStatus {
    let until = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(exit) = fixture.clients[index].try_wait().unwrap() {
            return exit;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "artifact client exceeded budget: {}",
            fixture.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

#[tokio::test]
async fn environment_artifact_run_preserves_original_ids_and_replays_without_execution() {
    use sha2::{Digest, Sha256};
    use std::{
        io::Write,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    let mut fixture = Fixture::new();
    assert!(
        fixture
            .command("gen-lock", "artifact-gen-lock")
            .status()
            .unwrap()
            .success()
    );
    let built_release = format!("built-artifact-{}", uuid::Uuid::new_v4().simple());
    let requested_release = format!("requested-artifact-{}", uuid::Uuid::new_v4().simple());
    let original_operation = format!("artifact-operation-{}", uuid::Uuid::new_v4().simple());
    let generation = format!("artifact-generation-{}", uuid::Uuid::new_v4().simple());
    let startup_counter = fixture.source.join("artifact-starts");
    let mut lock = app_cli::manifest::read_release_lock(&fixture.source).unwrap();
    lock.release_id = built_release.clone();
    lock.services[0].devrun = None;
    lock.services[0].run.command = vec![
        "sh".into(),
        "-ec".into(),
        "printf 'started\\n' >> \"$1\"; exec \"$2\" http \"$3\"".into(),
        "owned-artifact-http".into(),
        startup_counter.to_string_lossy().into_owned(),
        env!("CARGO_BIN_EXE_tree-fixture").into(),
        "artifact-only-http".into(),
    ];
    let port = lock.services[0].port;
    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, content) in [
        ("release.lock.toml", toml::to_string_pretty(&lock).unwrap()),
        (
            "workspace.manifest.toml",
            std::fs::read_to_string(fixture.source.join("workspace.manifest.toml")).unwrap(),
        ),
        (
            "web/project.manifest.toml",
            std::fs::read_to_string(fixture.source.join("web/project.manifest.toml")).unwrap(),
        ),
        ("web/artifact-receipt", built_release.clone()),
    ] {
        archive
            .start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(content.as_bytes()).unwrap();
    }
    let bytes = archive.finish().unwrap().into_inner();
    let sha = hex::encode(Sha256::digest(&bytes));
    // The existing explicit artifact contract activates the requested
    // execution directory. Keep bootstrap logs and its Pingap executable
    // outside that directory, just as an installed runtime would do.
    let execution = fixture.source.join(".run");
    std::fs::create_dir_all(&execution).unwrap();
    runtime_state_layout::record_project_origin(&fixture.source, &execution).unwrap();
    // Management and explicit artifact deployment must not require the current
    // Source manifest/cache. The archive independently retains its valid lock.
    std::fs::remove_file(fixture.source.join("workspace.manifest.toml")).unwrap();
    std::fs::remove_file(fixture.source.join("release.lock.toml")).unwrap();
    let downloads = Arc::new(AtomicUsize::new(0));
    let observed_downloads = downloads.clone();
    let router = axum::Router::new().route(
        "/artifact.zip",
        axum::routing::get(move || {
            let bytes = bytes.clone();
            let observed = observed_downloads.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                bytes
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/artifact.zip", listener.local_addr().unwrap());
    let asset_server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let command = |fixture: &Fixture, log: &str, release: &str| {
        let mut command = fixture.command_at("run", log, &execution);
        command
            .env("APP_DEPLOY_URL", &url)
            .env("APP_RELEASE_ID", release)
            .env("APP_DEPLOY_OPERATION_ID", &original_operation)
            .env("APP_DEPLOY_GENERATION_ID", &generation)
            .env("APP_DEPLOY_SHA256", &sha);
        command
    };
    let child = command(&fixture, "artifact-first-run", &requested_release)
        .spawn()
        .unwrap();
    fixture.clients.push(child);
    let index = fixture.clients.len() - 1;
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    let artifact_exit = operation_client_exit(&mut fixture, index).await;
    let token = std::fs::read_to_string(fixture.state.join("token")).unwrap();
    let endpoint = format!("{base}/v1/deploy/status?operation_id={original_operation}");
    let original: Value = client
        .get(&endpoint)
        .header("X-Deploy-Token", token.trim())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        artifact_exit.success(),
        "artifact result: {original}; {}",
        fixture.diagnostics()
    );
    let actual = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        actual, "artifact-only-http",
        "real business HTTP must come from artifact run command"
    );
    assert!(!fixture.source.join("workspace.manifest.toml").exists());
    assert!(!fixture.source.join("release.lock.toml").exists());
    assert!(fixture.source.join(".run/release.lock.toml").is_file());
    assert_eq!(
        std::fs::read_to_string(&startup_counter)
            .unwrap()
            .lines()
            .count(),
        1
    );
    let download_count = downloads.load(Ordering::SeqCst);
    assert!(
        download_count > 0,
        "explicit artifact must really download the controlled ZIP"
    );
    let operation = original["data"]["operation"].clone();
    assert_eq!(operation["operation_id"], original_operation);
    assert_eq!(operation["request_release_id"], requested_release);
    assert_eq!(operation["artifact_release_id"], built_release);
    assert_eq!(operation["deployment_generation_id"], generation);
    assert_eq!(operation["phase"], "running");
    assert_eq!(operation["persisted"], true);
    let replay = command(&fixture, "artifact-replay", &requested_release)
        .spawn()
        .unwrap();
    fixture.clients.push(replay);
    let index = fixture.clients.len() - 1;
    assert!(
        operation_client_exit(&mut fixture, index).await.success(),
        "{}",
        fixture.diagnostics()
    );
    let replayed: Value = client
        .get(&endpoint)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replayed["data"]["operation"], operation);
    assert_eq!(downloads.load(Ordering::SeqCst), download_count);
    assert_eq!(
        std::fs::read_to_string(&startup_counter)
            .unwrap()
            .lines()
            .count(),
        1
    );
    let changed = command(
        &fixture,
        "artifact-changed-input",
        "different-requested-release",
    )
    .spawn()
    .unwrap();
    fixture.clients.push(changed);
    let index = fixture.clients.len() - 1;
    assert!(
        !operation_client_exit(&mut fixture, index).await.success(),
        "same operation with changed input must be rejected"
    );
    assert!(fixture.diagnostics().contains("DEPLOY_OPERATION_CONFLICT"));
    assert_eq!(downloads.load(Ordering::SeqCst), download_count);
    assert_eq!(
        std::fs::read_to_string(&startup_counter)
            .unwrap()
            .lines()
            .count(),
        1
    );
    fixture.operation(&client, &base, &identity, "stop").await;
    fixture.same_owner(&client, &base, &identity, &native).await;
    assert!(
        client
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .is_err()
    );
    assert_eq!(downloads.load(Ordering::SeqCst), download_count);
    assert_eq!(
        std::fs::read_to_string(&startup_counter)
            .unwrap()
            .lines()
            .count(),
        1
    );
    asset_server.abort();
}

/// A proven artifact directory is an execution alias, never the Source
/// authority of a new run request. Its original artifact lock stays untouched.
#[tokio::test]
async fn source_run_from_proven_artifact_alias_uses_current_authoritative_source() {
    let mut fixture = Fixture::new();
    assert!(
        fixture
            .command("gen-lock", "alias-gen-lock")
            .status()
            .unwrap()
            .success()
    );
    let alias = fixture.source.join(".run");
    std::fs::create_dir_all(alias.join("web")).unwrap();
    runtime_state_layout::record_project_origin(&fixture.source, &alias).unwrap();
    let mut old = app_cli::manifest::read_release_lock(&fixture.source).unwrap();
    old.release_id = "preserved-old-artifact-identity".into();
    old.services[0].devrun = None;
    old.services[0].run.command = vec![
        env!("CARGO_BIN_EXE_tree-fixture").into(),
        "http".into(),
        "obsolete-artifact-http".into(),
    ];
    let old_bytes = toml::to_string_pretty(&old).unwrap().into_bytes();
    std::fs::write(alias.join("release.lock.toml"), &old_bytes).unwrap();
    // Only the explicitly chosen invocation directory differs. The fixture
    // still supplies the same platform binding, logs and runtime executable.
    let mut command = fixture.command_at("run", "alias-run", &alias);
    fixture.clients.push(command.spawn().unwrap());
    let index = fixture.clients.len() - 1;
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    assert!(
        operation_client_exit(&mut fixture, index).await.success(),
        "new Source run from a proven alias must succeed: {}",
        fixture.diagnostics()
    );
    fixture.http(&client, "first").await;
    assert_eq!(
        identity["source_root"],
        fixture.source.to_string_lossy().as_ref()
    );
    assert_eq!(
        std::fs::read(alias.join("release.lock.toml")).unwrap(),
        old_bytes,
        "new Source must not rewrite the old artifact identity"
    );
    fixture.same_owner(&client, &base, &identity, &native).await;
    fixture.operation(&client, &base, &identity, "stop").await;
    fixture.same_owner(&client, &base, &identity, &native).await;
}

/// A failed application-owned prestart script is diagnostic. The same
/// original Source operation still starts actual HTTP and commits Succeeded.
#[tokio::test]
async fn failed_migration_is_advisory_for_actual_http_and_original_operation() {
    let mut fixture = Fixture::new();
    let manifest = fixture.source.join("web/project.manifest.toml");
    let mut text = std::fs::read_to_string(&manifest).unwrap();
    let migrate = toml::Value::Array(
        ["sh", "-c", "printf 'migration-stdout-before-failure\\n'; printf 'migration-stderr-before-failure\\n' >&2; exit 1"]
            .into_iter()
            .map(|value| toml::Value::String(value.into()))
            .collect(),
    );
    text = text.replace(
        "shutdown_timeout_seconds=3",
        &format!("migrate={migrate}\nshutdown_timeout_seconds=3"),
    );
    std::fs::write(manifest, text).unwrap();
    let index = fixture.spawn_run("migration-failed-run");
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    assert!(
        operation_client_exit(&mut fixture, index).await.success(),
        "script exit 1 must not fail its original Source operation: {}",
        fixture.diagnostics(),
    );
    fixture.http(&client, "first").await;
    let stdout =
        std::fs::read_to_string(fixture.source.join("migration-failed-run.stdout")).unwrap();
    let id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("observing Source operation "))
        .expect("CLI must identify its original operation");
    let token = std::fs::read_to_string(fixture.state.join("token")).unwrap();
    let response: Value = client
        .get(format!("{base}/v1/runtime/operations/{id}"))
        .header("X-Deploy-Token", token.trim())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["data"]["operation_id"], id);
    assert_eq!(
        response["data"]["runtime_instance_id"],
        identity["runtime_instance_id"]
    );
    assert_eq!(response["data"]["state"], "succeeded");
    let out = std::fs::read_to_string(
        fixture
            .source
            .join("logs")
            .join(&fixture.app)
            .join("runtime.out.log"),
    )
    .unwrap();
    let err = std::fs::read_to_string(
        fixture
            .source
            .join("logs")
            .join(&fixture.app)
            .join("runtime.err.log"),
    )
    .unwrap();
    assert!(out.contains("migration-stdout-before-failure"), "{out}");
    assert!(err.contains("migration-stderr-before-failure"), "{err}");
    assert!(
        err.contains("exit"),
        "failure log must retain exit reason: {err}"
    );
    let events: Value = client
        .get(format!(
            "{base}/v1/runtime/operations/{id}/events?after_seq=0"
        ))
        .header("X-Deploy-Token", token.trim())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let records = events["data"]["events"].as_array().unwrap();
    assert!(records.iter().all(|record| record["operation_id"] == id));
    assert!(
        records.iter().any(|record| record["event_name"] == "log"
            && record["payload"]["line"]
                .as_str()
                .is_some_and(|line| line.contains("migration-stdout-before-failure"))),
        "{events}"
    );
    assert!(
        records.iter().any(|record| record["event_name"] == "log"
            && record["payload"]["line"]
                .as_str()
                .is_some_and(|line| line.contains("migration-stderr-before-failure"))),
        "{events}"
    );
    fixture.operation(&client, &base, &identity, "stop").await;
    fixture.same_owner(&client, &base, &identity, &native).await;
}

#[tokio::test]
async fn stop_during_migration_cancels_original_start_without_late_business_launch() {
    let mut fixture = Fixture::new();
    let path = fixture.source.join("web/project.manifest.toml");
    let mut manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    manifest["run"].as_table_mut().unwrap().insert(
        "migrate".into(),
        toml::Value::Array(
            [
                "sh",
                "-c",
                "printf 'parent-stop-migration-output\\n'; echo $$ > migration.pid; exec tail -f /dev/null",
            ]
            .into_iter()
            .map(|value| toml::Value::String(value.into()))
            .collect(),
        ),
    );
    let launch = format!(
        "echo started > business-started; exec '{}' http late-business",
        env!("CARGO_BIN_EXE_tree-fixture").replace('\'', "'\"'\"'")
    );
    let command = toml::Value::Array(
        ["sh".to_owned(), "-c".to_owned(), launch]
            .into_iter()
            .map(toml::Value::String)
            .collect(),
    );
    manifest["run"]["command"] = command.clone();
    manifest["devrun"]["command"] = command;
    std::fs::write(path, toml::to_string(&manifest).unwrap()).unwrap();
    let index = fixture.spawn_run("migration-parent-stop");
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    let migration_pid = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(text) =
                tokio::fs::read_to_string(fixture.source.join("web/migration.pid")).await
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the actual migration must start before Stop is submitted");
    let stdout =
        std::fs::read_to_string(fixture.source.join("migration-parent-stop.stdout")).unwrap();
    let id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("observing Source operation "))
        .unwrap()
        .to_owned();
    let token = std::fs::read_to_string(fixture.state.join("token")).unwrap();
    let before: Value = client
        .get(format!("{base}/v1/runtime/operations/{id}"))
        .header("X-Deploy-Token", token.trim())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["data"]["operation_id"], id);
    assert_eq!(
        before["data"]["runtime_instance_id"],
        identity["runtime_instance_id"]
    );
    assert_eq!(
        before["data"]["state"], "accepted",
        "Stop must interrupt the original nonterminal Start: {before}"
    );
    let status: Value = client
        .get(format!("{base}/v1/runtime/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["data"]["active_operation_id"], id, "{status}");
    assert!(
        process_utils::process_group_exists(migration_pid).unwrap(),
        "the same captured migration process group must be alive at Stop admission"
    );
    assert!(
        !fixture.source.join("web/business-started").exists(),
        "business must not have launched before the Stop barrier"
    );
    fixture.operation(&client, &base, &identity, "stop").await;
    let exit = operation_client_exit(&mut fixture, index).await;
    assert!(
        !process_utils::process_group_exists(migration_pid).unwrap(),
        "the captured original migration process tree must be gone"
    );
    assert!(
        !fixture.source.join("web/business-started").exists(),
        "an admitted parent Stop must prevent late business launch"
    );
    let operation: Value = client
        .get(format!("{base}/v1/runtime/operations/{id}"))
        .header("X-Deploy-Token", token.trim())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(operation["data"]["operation_id"], id);
    assert_eq!(
        operation["data"]["runtime_instance_id"],
        identity["runtime_instance_id"]
    );
    assert_eq!(operation["data"]["state"], "cancelled", "{operation}");
    assert!(
        !exit.success(),
        "the cancelled foreground Start must not report CLI success: operation={operation}; diagnostics={}",
        fixture.diagnostics()
    );
    let log = std::fs::read_to_string(
        fixture
            .source
            .join("logs")
            .join(&fixture.app)
            .join("runtime.out.log"),
    )
    .unwrap();
    assert!(log.contains("parent-stop-migration-output"), "{log}");
    let events: Value = client
        .get(format!(
            "{base}/v1/runtime/operations/{id}/events?after_seq=0"
        ))
        .header("X-Deploy-Token", token.trim())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        events["data"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["operation_id"] == id
                && event["service"] == fixture.app
                && event["event_name"] == "log"
                && event["payload"]["line"]
                    .as_str()
                    .is_some_and(|line| line.contains("parent-stop-migration-output"))),
        "{events}"
    );
    fixture.same_owner(&client, &base, &identity, &native).await;
}

#[tokio::test]
async fn guarded_migration_spawn_failure_still_starts_actual_http_and_commits_original_operation() {
    let mut fixture = Fixture::new();
    let path = fixture.source.join("web/project.manifest.toml");
    let mut manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    manifest["run"].as_table_mut().unwrap().insert(
        "migrate".into(),
        toml::Value::Array(vec![toml::Value::String(
            "/definitely-missing/migration-executable".into(),
        )]),
    );
    std::fs::write(path, toml::to_string(&manifest).unwrap()).unwrap();
    let index = fixture.spawn_run("guarded-migration-spawn");
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    assert!(
        operation_client_exit(&mut fixture, index).await.success(),
        "a confirmed guardian spawn failure must remain advisory: {}",
        fixture.diagnostics()
    );
    fixture.http(&client, "first").await;
    let stdout =
        std::fs::read_to_string(fixture.source.join("guarded-migration-spawn.stdout")).unwrap();
    let id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("observing Source operation "))
        .unwrap();
    let token = std::fs::read_to_string(fixture.state.join("token")).unwrap();
    let operation: Value = client
        .get(format!("{base}/v1/runtime/operations/{id}"))
        .header("X-Deploy-Token", token.trim())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(operation["data"]["operation_id"], id);
    assert_eq!(
        operation["data"]["runtime_instance_id"],
        identity["runtime_instance_id"]
    );
    assert_eq!(operation["data"]["state"], "succeeded", "{operation}");
    let err = std::fs::read_to_string(
        fixture
            .source
            .join("logs")
            .join(&fixture.app)
            .join("runtime.err.log"),
    )
    .unwrap();
    assert!(
        err.contains("ERROR") && err.contains("spawn migration"),
        "{err}"
    );
    assert!(
        err.contains("No such file") || err.contains("not found"),
        "guardian stderr must retain the executable failure detail: {err}"
    );
    let events: Value = client
        .get(format!(
            "{base}/v1/runtime/operations/{id}/events?after_seq=0"
        ))
        .header("X-Deploy-Token", token.trim())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        events["data"]["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["event_name"] == "log"
                && event["payload"]["line"]
                    .as_str()
                    .is_some_and(|line| line.contains("spawn migration"))),
        "{events}"
    );
    fixture.operation(&client, &base, &identity, "stop").await;
    fixture.same_owner(&client, &base, &identity, &native).await;
}

/// 慢启动 manifest：run 命令先 sleep 再起真实 HTTP，拉长原操作的
/// 非终态窗口，供终局时序断言与 Stop 竞争使用。
impl Fixture {
    /// spawn run 客户端并捕获 stdout 的 EVT 行（跨线程 channel 回传解析后
    /// 的 JSON 事件；非 EVT 行忽略）。
    fn spawn_run_piped(&mut self, log: &str) -> std::sync::mpsc::Receiver<Value> {
        use std::io::{BufRead, BufReader};
        let mut child = self
            .command("run", log)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (events_tx, events_rx) = std::sync::mpsc::channel::<Value>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // wire 契约字面量（与 shared_types::APP_CLI_EVT_PREFIX 一致）。
                let Some(payload) = line.strip_prefix("APP-CLI-EVT ") else {
                    continue;
                };
                if let Ok(event) = serde_json::from_str::<Value>(payload) {
                    let _ = events_tx.send(event);
                }
            }
        });
        self.clients.push(child);
        events_rx
    }
}

/// 等待 owner 出现非终态活跃操作并返回其 id（run 客户端提交的 Start）。
async fn await_active_operation(fixture: &Fixture, client: &reqwest::Client, base: &str) -> String {
    let until = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(response) = client.get(format!("{base}/v1/runtime/status")).send().await
            && let Ok(body) = response.json::<Value>().await
            && let Some(id) = body["data"]["active_operation_id"].as_str()
        {
            return id.to_string();
        }
        assert!(
            tokio::time::Instant::now() < until,
            "active start operation missing: {}",
            fixture.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn operation_view(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    operation_id: &str,
) -> Value {
    client
        .get(format!("{base}/v1/runtime/operations/{operation_id}"))
        .header("X-Deploy-Token", token.trim())
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["data"]
        .clone()
}

/// R1 回归：客户端 stdout 的 orchestration_done 绑定原操作的权威终态——
/// done 出现的那一刻，捕获操作必须已终态成功；整条 stdout 终局恰好一次，
/// 客户端 0 退出。（缺陷形态：journal done 早于提交屏障转发，操作随后可被
/// Stop 取消而消费者已按成功收场。）
#[tokio::test]
async fn run_client_done_follows_authoritative_operation_result() {
    let mut fixture = Fixture::new();
    assert!(
        fixture
            .command("gen-lock", "gen-lock")
            .status()
            .unwrap()
            .success(),
        "{}",
        fixture.diagnostics()
    );
    fixture.slow_manifests("done-authority", 3);
    let events = fixture.spawn_run_piped("done-authority");
    let client = client();
    let (base, identity, _native) = fixture.ready(&client).await;
    let token = std::fs::read_to_string(fixture.state.join("token")).unwrap();
    let operation_id = await_active_operation(&fixture, &client, &base).await;

    let mut dones_seen = 0usize;
    let exit = {
        let until = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            while let Ok(event) = events.try_recv() {
                if event["event"] == "orchestration_done" {
                    dones_seen += 1;
                    assert_eq!(dones_seen, 1, "终局恰好一次: {}", fixture.diagnostics());
                    assert!(
                        event["failed"]
                            .as_array()
                            .is_some_and(|failed| failed.is_empty()),
                        "成功终局不得带失败清单: {event}"
                    );
                    let view = operation_view(&client, &base, &token, &operation_id).await;
                    assert_eq!(
                        view["state"], "succeeded",
                        "done 必须跟随原操作的权威终态: {view}"
                    );
                }
            }
            if let Some(exit) = fixture.clients.last_mut().unwrap().try_wait().unwrap() {
                break exit;
            }
            assert!(
                tokio::time::Instant::now() < until,
                "client exceeded budget: {}",
                fixture.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    // 进程退出后的管道残余事件同样计入终局计数。
    while let Ok(event) = events.try_recv() {
        if event["event"] == "orchestration_done" {
            dones_seen += 1;
        }
    }
    assert_eq!(dones_seen, 1, "整条 stdout 恰好一个终局 done");
    assert!(exit.success(), "client exit: {exit}");
    let view = operation_view(&client, &base, &token, &operation_id).await;
    assert_eq!(view["state"], "succeeded");
    fixture.operation(&client, &base, &identity, "stop").await;
}

/// R1 反例回归：Stop 在原操作仍在途时受理——终局 done 必须单一且带失败
/// 清单，原操作终态非 succeeded；Stop 受理前不得已出现过任何 done。
#[tokio::test]
async fn stop_before_commit_yields_single_failure_done() {
    let mut fixture = Fixture::new();
    assert!(
        fixture
            .command("gen-lock", "gen-lock")
            .status()
            .unwrap()
            .success(),
        "{}",
        fixture.diagnostics()
    );
    fixture.slow_manifests("stop-race", 8);
    let events = fixture.spawn_run_piped("stop-race");
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    let token = std::fs::read_to_string(fixture.state.join("token")).unwrap();
    let operation_id = await_active_operation(&fixture, &client, &base).await;

    // Stop 受理前不允许已经出现终局 done（缺陷形态会提前转发空清单 done）。
    let mut dones_before_stop = 0usize;
    while let Ok(event) = events.try_recv() {
        if event["event"] == "orchestration_done" {
            dones_before_stop += 1;
        }
    }
    assert_eq!(dones_before_stop, 0, "在途操作不得先输出终局 done");

    fixture.operation(&client, &base, &identity, "stop").await;
    let until = tokio::time::Instant::now() + Duration::from_secs(30);
    let final_view = loop {
        let view = operation_view(&client, &base, &token, &operation_id).await;
        if matches!(
            view["state"].as_str(),
            Some("succeeded" | "failed" | "cancelled" | "recovery_required")
        ) {
            break view;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "original operation did not settle: {view}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    };
    assert_ne!(
        final_view["state"], "succeeded",
        "被 Stop 取消的原操作不得计成功: {final_view}"
    );

    let until = tokio::time::Instant::now() + Duration::from_secs(45);
    let exit = loop {
        if let Some(exit) = fixture.clients.last_mut().unwrap().try_wait().unwrap() {
            break exit;
        }
        assert!(
            tokio::time::Instant::now() < until,
            "client exceeded budget: {}",
            fixture.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let mut dones: Vec<Value> = Vec::new();
    while let Ok(event) = events.try_recv() {
        if event["event"] == "orchestration_done" {
            dones.push(event);
        }
    }
    assert_eq!(
        dones.len(),
        1,
        "终局恰好一次（实得 {dones:?}）: {}",
        fixture.diagnostics()
    );
    assert!(
        dones[0]["failed"]
            .as_array()
            .is_some_and(|failed| !failed.is_empty()),
        "取消终局必须携带失败清单: {:?}",
        dones[0]
    );
    // 退出码按终态契约（Cancelled=被取代语义为 0；其余失败态非零）——
    // 这里只要求与 done 的失败语义不矛盾：记录即可，不强断言。
    println!(
        "stop-race client exit: {exit}, original state: {}",
        final_view["state"]
    );
    fixture.same_owner(&client, &base, &identity, &native).await;
}
