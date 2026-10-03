//! A real control-only CLI reuses a stopped owner's management plane, even
//! when source configuration is missing or malformed. It cannot submit Start.
#![cfg(unix)]

use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn spawn(source: &Path, state: &Path, log: &str) -> Process {
    let mut command = Command::new(env!("CARGO_BIN_EXE_app-cli"));
    command.args(["serve", "--control-only", "--workspace"]).arg(source)
        .args(["--admin-addr", "127.0.0.1:0", "--log-dir"]).arg(source.join("logs"))
        .env("PROJECT_ID", "reuse-test").env("APP_CLI_STATE_ROOT", state)
        .env("SERVICE_TYPE", "userapp-builder").env("APP_CLI_MANAGED", "1")
        .env("APP_CLI_RUNTIME_WORKSPACE", source).env("APP_CLI_REQUIRE_PG", "0")
        .env("APP_CLI_DEPLOY_TOKEN", "reuse-fixture-token")
        .env_remove("APP_DEPLOY_URL").env_remove("APP_DEPLOY_OPERATION_ID")
        .env_remove("APP_DEPLOY_GENERATION_ID").env_remove("APP_RELEASE_ID")
        .env("RCODER_EXECUTION_DOMAIN", json!({"authority":"management-reuse-tests", "volume":source.to_str().unwrap(), "instance":"same-process-space", "instance_source_env":null}).to_string())
        .stdin(Stdio::null()).stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(source.join(log)).unwrap()));
    Process(command.spawn().unwrap())
}

async fn ready(process: &mut Process, client: &reqwest::Client, state: &Path) -> (String, Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "owner exited before management readiness"
        );
        if let Ok(bytes) = std::fs::read(state.join("endpoint.json"))
            && let Ok(endpoint) = serde_json::from_slice::<Value>(&bytes)
            && let Some(address) = endpoint["address"].as_str()
            && let Ok(native) = runtime_supervisor::last_snapshot(state)
            && native.phase == runtime_supervisor::Phase::Ready
        {
            let base = format!("http://{address}");
            if let Ok(response) = client
                .get(format!("{base}/v1/runtime/identity"))
                .send()
                .await
                && let Ok(body) = response.json::<Value>().await
                && body["data"]["runtime_instance_id"].is_string()
            {
                return (base, body["data"].clone());
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "management not ready: {:?}",
            runtime_supervisor::last_snapshot(state)
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

fn operation_files(state: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(state.join("operations"))
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().to_string(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn repeated_control_only_preserves_stopped_owner_and_operations_without_source_lock() {
    for malformed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().canonicalize().unwrap().join("reuse-test");
        let state = source.join("state/reuse-test");
        std::fs::create_dir_all(&source).unwrap();
        if malformed {
            std::fs::write(source.join("release.lock.toml"), "not valid TOML = [").unwrap();
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let mut original = spawn(&source, &state, "original.stderr");
        let (base, identity) = ready(&mut original, &client, &state).await;
        let status: Value = client
            .get(format!("{base}/v1/runtime/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let response = client.post(format!("{base}/v1/runtime/operations"))
            .header("X-Deploy-Token", "reuse-fixture-token")
            .json(&json!({"operation_id":"explicit-stop", "expected_runtime_instance_id":identity["runtime_instance_id"], "expected_revision":status["data"]["revision"], "workspace_id":identity["workspace_id"], "kind":"stop", "profile":{"profile":"source", "input":{"workspace_id":identity["workspace_id"]}}}))
            .send().await.unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::ACCEPTED,
            "{}",
            response.text().await.unwrap()
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let body: Value = client
                .get(format!("{base}/v1/runtime/operations/explicit-stop"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if body["data"]["state"] == "succeeded" {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "stop did not settle: {body}"
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let _ = ready(&mut original, &client, &state).await;
        let operations = operation_files(&state);
        let desired = std::fs::read(state.join("desired.json")).unwrap();
        let native = runtime_supervisor::last_snapshot(&state).unwrap();
        let mut repeated = spawn(&source, &state, "repeated.stderr");
        let exit = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(exit) = repeated.0.try_wait().unwrap() {
                    break exit;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        })
        .await
        .expect("control-only reuse must return without starting business");
        assert!(
            exit.success(),
            "control-only reuse failed: {}",
            std::fs::read_to_string(source.join("repeated.stderr")).unwrap()
        );
        assert!(original.0.try_wait().unwrap().is_none());
        assert_eq!(std::fs::read(state.join("desired.json")).unwrap(), desired);
        assert_eq!(
            operation_files(&state),
            operations,
            "management reuse must not submit cli-dispatch Source Start"
        );
        let after = runtime_supervisor::last_snapshot(&state).unwrap();
        assert_eq!(after.supervisor_id, native.supervisor_id);
        assert_eq!(after.generation, native.generation);
        assert_eq!(after.intent, native.intent);
        let after_status: Value = client
            .get(format!("{base}/v1/runtime/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(after_status["data"]["desired"], "stopped");
    }
}
