//! Real same-authority owner replacement. No project files are relocated.
#![cfg(unix)]

use serde_json::{Value, json};
use std::{
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

fn spawn(workspace: &Path, source: &Path, state: &Path, managed: bool) -> Process {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_app-cli"));
    cmd.args(["serve", "--workspace"])
        .arg(workspace)
        .current_dir(workspace)
        .args(["--admin-addr", "127.0.0.1:0", "--control-only", "--log-dir"])
        .arg(source.join("logs"))
        .env("PROJECT_ID", "managed-test")
        .env("APP_CLI_STATE_ROOT", state)
        .env("APP_CLI_REQUIRE_PG", "0")
        .env("APP_CLI_DEPLOY_TOKEN", "managed-test-token")
        .env_remove("APP_DEPLOY_URL")
        .env_remove("APP_DEPLOY_OPERATION_ID")
        .env_remove("APP_DEPLOY_GENERATION_ID")
        .env_remove("APP_RELEASE_ID")
        .env_remove("SERVICE_TYPE")
        .env_remove("APP_CLI_MANAGED")
        .env_remove("APP_CLI_RUNTIME_WORKSPACE")
        .env(
            "RCODER_EXECUTION_DOMAIN",
            json!({
                "authority":"managed-owner-tests", "volume":source.to_str().unwrap(),
                "instance":"same-process-space", "instance_source_env":null,
            })
            .to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(source.join(if managed { "new.stderr" } else { "old.stderr" }))
                .unwrap(),
        ));
    if managed {
        cmd.env("SERVICE_TYPE", "userapp-builder")
            .env("APP_CLI_MANAGED", "1")
            .env("APP_CLI_RUNTIME_WORKSPACE", source);
    }
    Process(cmd.spawn().unwrap())
}

async fn ready(
    process: &mut Process,
    client: &reqwest::Client,
    state: &Path,
    expected: &Path,
) -> (String, Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "owner exited before becoming ready"
        );
        if let Ok(bytes) = std::fs::read(state.join("endpoint.json"))
            && let Ok(endpoint) = serde_json::from_slice::<Value>(&bytes)
            && let Some(address) = endpoint["address"].as_str()
        {
            let base = format!("http://{address}");
            if let Ok(response) = client
                .get(format!("{base}/v1/runtime/identity"))
                .send()
                .await
                && let Ok(body) = response.json::<Value>().await
                && body["data"]["source_root"].as_str() == expected.to_str()
                && let Ok(native) = runtime_supervisor::last_snapshot(state)
                && native.binding.resource == expected
                && native.phase == runtime_supervisor::Phase::Ready
            {
                return (base, body["data"].clone());
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "management recovery timed out for {}: native={:?}; identity={}; logs={}",
            expected.display(),
            runtime_supervisor::last_snapshot(state),
            std::fs::read_to_string(state.join("identity.json")).unwrap_or_default(),
            std::fs::read_to_string(state.parent().unwrap().parent().unwrap().join("old.stderr"))
                .unwrap_or_default(),
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

async fn operation(
    client: &reqwest::Client,
    base: &str,
    identity: &Value,
    id: &str,
    kind: &str,
) -> Value {
    let status: Value = client
        .get(format!("{base}/v1/runtime/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let response = client.post(format!("{base}/v1/runtime/operations"))
        .header("X-Deploy-Token", "managed-test-token")
        .json(&json!({
            "operation_id":id, "expected_runtime_instance_id":identity["runtime_instance_id"],
            "expected_revision":status["data"]["revision"], "workspace_id":identity["workspace_id"],
            "kind":kind, "profile":{"profile":"source","input":{"workspace_id":identity["workspace_id"],"dev":true}},
        })).send().await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{body}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let body: Value = client
            .get(format!("{base}/v1/runtime/operations/{id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let view = &body["data"];
        if matches!(
            view["state"].as_str(),
            Some("succeeded" | "failed" | "cancelled" | "recovery_required")
        ) {
            return view.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "operation stuck: {body}"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

async fn recover(live: bool, corrupt_identity: bool, moved_source: bool) {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().canonicalize().unwrap().join("managed-test");
    let wrong = source.join("misplaced-project");
    let state = source.join("state/managed-test");
    std::fs::create_dir_all(&wrong).unwrap();
    std::fs::write(wrong.join("source-sentinel.txt"), "preserve source").unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let mut old = spawn(&wrong, &source, &state, false);
    let (base, old_identity) = ready(&mut old, &client, &state, &wrong).await;
    let stopped = operation(&client, &base, &old_identity, "original-stop", "stop").await;
    assert_eq!(stopped["state"], "succeeded");
    // Capture a real running management generation, even though business is stopped.
    let _ = ready(&mut old, &client, &state, &wrong).await;
    let captured = runtime_supervisor::last_snapshot(&state).unwrap();
    let generation = captured.generation.clone().unwrap();
    let operation_bytes = std::fs::read(state.join("operations/original-stop.json")).unwrap();
    if !live {
        old.0.kill().unwrap();
        old.0.wait().unwrap();
        let receipt: Value = serde_json::from_slice(
            &std::fs::read(state.join("work").join(&generation).join("generation.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            receipt["phase"], "Running",
            "fault must leave an unfinished generation"
        );
    }
    if corrupt_identity {
        std::fs::write(state.join("identity.json"), "{interrupted").unwrap();
    }
    // Simulate an agent's explicit source repair. The platform recovery itself
    // must not move any files and must not require the old process cwd to exist.
    let preserved_source = if moved_source {
        let repaired = source.join("adapted-source");
        std::fs::rename(&wrong, &repaired).unwrap();
        repaired
    } else {
        wrong.clone()
    };
    let mut replacement = spawn(&source, &source, &state, true);
    let (new_base, identity) = ready(&mut replacement, &client, &state, &source).await;
    assert_ne!(
        identity["runtime_instance_id"],
        old_identity["runtime_instance_id"]
    );
    if !corrupt_identity {
        assert_eq!(identity["workspace_id"], old_identity["workspace_id"]);
    }
    assert_eq!(
        std::fs::read(state.join("operations/original-stop.json")).unwrap(),
        operation_bytes
    );
    assert_eq!(
        std::fs::read_to_string(preserved_source.join("source-sentinel.txt")).unwrap(),
        "preserve source"
    );
    assert!(
        !source.join("source-sentinel.txt").exists(),
        "recovery must not relocate source"
    );
    let proof = runtime_supervisor::verify_local_quiescent(&state, &generation)
        .unwrap()
        .unwrap();
    assert_eq!(proof.supervisor_id, captured.supervisor_id);
    let error = runtime_supervisor::control_verified(
        &state,
        runtime_supervisor::Request::new(runtime_supervisor::Action::Shutdown),
        &captured.supervisor_id,
    )
    .await
    .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<runtime_supervisor::Problem>()
            .unwrap()
            .code,
        runtime_supervisor::FailureCode::IdentityChanged
    );
    assert!(replacement.0.try_wait().unwrap().is_none());
    assert_eq!(
        operation(&client, &new_base, &identity, "new-stop", "stop").await["state"],
        "succeeded"
    );
    // Empty source is an honest configuration failure, not a stale-owner gate.
    let start = operation(&client, &new_base, &identity, "new-start", "start").await;
    assert_eq!(start["state"], "failed", "{start}");
    assert_eq!(start["error_code"], "ERR_VALIDATION", "{start}");
}

#[tokio::test]
async fn killed_same_app_owner_is_reconciled_before_source_root_start() {
    recover(false, false, false).await;
}

#[tokio::test]
async fn damaged_identity_does_not_disable_management_or_next_start() {
    recover(false, true, false).await;
}

#[tokio::test]
async fn live_wrong_root_owner_is_shutdown_before_new_owner_bootstraps() {
    recover(true, false, false).await;
}

#[tokio::test]
async fn project_repair_can_remove_the_live_owners_old_working_directory() {
    recover(true, false, true).await;
}
