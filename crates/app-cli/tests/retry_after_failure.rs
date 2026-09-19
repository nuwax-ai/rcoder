//! 2026-09-19 批 9 反例：启动失败后的重试必须真实执行（batch8-followup §3）。
//!
//! 场景：注入可控启动失败（run command 恒失败）→ Start 收束 Failed →
//! ① 同操作 ID 重放必须返回原 Failed 结果（幂等，不重新执行也不瞬完成）；
//! ② 新操作 ID 必须真实再次执行（观察到新事件序列并再次 Failed），
//!    绝不出现"瞬时 completed 而无执行证据"。
#![cfg(unix)]

use std::{
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct OwnedServer(Child);
impl Drop for OwnedServer {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// 失败注入工作区：单服务，run command 恒 exit 7（readiness 永不通过）。
fn failing_workspace(root: &Path) -> std::path::PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("svc")).expect("svc dir");
    std::fs::write(
        workspace.join("workspace.manifest.toml"),
        "schema_version = 1\n\n[workspace]\nname = \"retry-probe\"\n",
    )
    .expect("ws manifest");
    std::fs::write(
        workspace.join("svc/project.manifest.toml"),
        "schema_version = 1\n\n[project]\nservice_id = \"svc\"\nname = \"Failing\"\ntype = \"node\"\nkind = \"web\"\nenabled = true\n\n[build]\ncommand = [\"true\"]\nartifact = \"out.txt\"\n\n[run]\ncommand = [\"/bin/sh\", \"-c\", \"exit 7\"]\n\n[health]\nreadiness_path = \"/ready\"\n\n[proxy]\npath = \"/api/svc/\"\nstrip_prefix = true\n",
    )
    .expect("svc manifest");
    workspace
}

fn spawn_owner(workspace: &Path, logs: &Path, token: &str) -> (OwnedServer, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve admin");
    let address = listener.local_addr().expect("test address");
    drop(listener);
    let child = Command::new(env!("CARGO_BIN_EXE_app-cli"))
        .args([
            "serve",
            "--workspace",
            workspace.to_str().expect("workspace path"),
            "--log-dir",
            logs.to_str().expect("log path"),
            "--admin-addr",
            &address.to_string(),
        ])
        .env("APP_CLI_DEPLOY_TOKEN", token)
        .env("RUST_LOG", "info")
        .stderr(Stdio::from(
            std::fs::File::create(logs.join("owner.err")).expect("stderr log"),
        ))
        .env_remove("APP_DEPLOY_URL")
        .env_remove("APP_RELEASE_ID")
        .env_remove("APP_DEPLOY_OPERATION_ID")
        .env_remove("APP_DEPLOY_GENERATION_ID")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn owned app-cli");
    (OwnedServer(child), format!("http://{address}"))
}

async fn wait_identity(base: &str, token: &str) -> serde_json::Value {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = client
            .get(format!("{base}/v1/runtime/identity"))
            .header("x-deploy-token", token)
            .send()
            .await
            && response.status().is_success()
        {
            let value: serde_json::Value = response.json().await.unwrap();
            if value.get("data").is_some() {
                return value["data"].clone();
            }
        }
        assert!(Instant::now() < deadline, "owner API did not start");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// identity 视图不含 revision——从 /v1/runtime/status 读出并合并。
async fn with_revision(
    base: &str,
    token: &str,
    mut identity: serde_json::Value,
) -> serde_json::Value {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = client
            .get(format!("{base}/v1/runtime/status"))
            .header("x-deploy-token", token)
            .send()
            .await
            && response.status().is_success()
        {
            let status: serde_json::Value = response.json().await.unwrap_or_default();
            let data = status.get("data").cloned().unwrap_or_default();
            if let Some(revision) = data.get("revision").and_then(|v| v.as_u64()) {
                identity["revision"] = serde_json::json!(revision);
                return identity;
            } else if let Some(revision) = status.get("revision").and_then(|v| v.as_u64()) {
                identity["revision"] = serde_json::json!(revision);
                return identity;
            }
        }
        assert!(
            Instant::now() < deadline,
            "runtime status did not report revision"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn submit_start(
    base: &str,
    token: &str,
    identity: &serde_json::Value,
    operation_id: &str,
) -> Result<serde_json::Value, serde_json::Value> {
    let body = serde_json::json!({
        "operation_id": operation_id,
        "expected_runtime_instance_id": identity["runtime_instance_id"],
        "expected_revision": identity["revision"],
        "workspace_id": identity["workspace_id"],
        "kind": "start",
        "profile": { "profile": "source", "input": { "workspace_id": "workspace" } },
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let response = client
        .post(format!("{base}/v1/runtime/operations"))
        .header("x-deploy-token", token)
        .json(&body)
        .send()
        .await
        .expect("submit start");
    let status = response.status();
    let value: serde_json::Value = response.json().await.unwrap();
    if status.is_success() {
        Ok(value)
    } else {
        Err(value)
    }
}

async fn poll_terminal(
    base: &str,
    token: &str,
    operation_id: &str,
    logs_path: &Path,
) -> (String, serde_json::Value) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let Ok(response) = client
            .get(format!("{base}/v1/runtime/operations/{operation_id}"))
            .header("x-deploy-token", token)
            .send()
            .await
        else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let value: serde_json::Value = response.json().await.unwrap_or_default();
        let state = value["data"]["state"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        match state.as_str() {
            "failed" | "cancelled" | "succeeded" | "recovery_required" => {
                return (state, value);
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            let tail = std::fs::read_to_string(logs_path.join("owner.err")).unwrap_or_default();
            let tail: String = tail.lines().rev().take(25).collect::<Vec<_>>().join("\n");
            panic!(
                "operation {operation_id} did not reach terminal: {value}\n--- owner.err tail:\n{tail}"
            );
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn event_count(base: &str, token: &str, operation_id: &str) -> usize {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(800))
        .build()
        .unwrap();
    let Ok(response) = client
        .get(format!(
            "{base}/v1/runtime/operations/{operation_id}/events"
        ))
        .header("x-deploy-token", token)
        .send()
        .await
    else {
        return 0;
    };
    let value: serde_json::Value = response.json().await.unwrap_or_default();
    value["data"]["events"]
        .as_array()
        .map(|events| events.len())
        .unwrap_or(0)
}

#[tokio::test]
async fn retry_after_startup_failure_reexecutes_and_replay_returns_original() {
    let root = tempfile::tempdir().unwrap();
    let workspace = failing_workspace(root.path());
    let logs = root.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    // 生成 release.lock.toml（serve 的 start 派发需要锁定的工作区目标）
    let gen_output = Command::new(env!("CARGO_BIN_EXE_app-cli"))
        .args([
            "gen-lock",
            "--workspace",
            workspace.to_str().expect("workspace path"),
        ])
        .env_remove("APP_DEPLOY_URL")
        .output()
        .expect("run gen-lock");
    assert!(
        gen_output.status.success(),
        "gen-lock must succeed for failing workspace: {}",
        String::from_utf8_lossy(&gen_output.stderr)
    );
    let (mut server, base) = spawn_owner(&workspace, &logs, "probe-token");
    let token = "probe-token";

    let identity = wait_identity(&base, token).await;
    let identity = with_revision(&base, token, identity).await;

    // 第一次 Start：注入的 run command 失败 → 真实 Failed
    let first = submit_start(&base, token, &identity, "op-fail-1")
        .await
        .expect("first admission must be accepted");
    assert_eq!(
        first["data"]["state"], "accepted",
        "first admission: {first}"
    );
    let (state, view) = poll_terminal(&base, token, "op-fail-1", &logs).await;
    assert!(
        state == "failed" || state == "recovery_required",
        "injected failure must surface as a real terminal (not instant success): {view}"
    );
    let first_events = event_count(&base, token, "op-fail-1").await;
    assert!(first_events >= 2, "first run must have execution evidence");

    // ① 同操作 ID 重放：返回原 Failed 结果（幂等），不重新执行
    let replay = submit_start(&base, token, &identity, "op-fail-1")
        .await
        .expect("same-id replay must be accepted (idempotent)");
    let replay_state = replay["data"]["state"].as_str().unwrap_or_default();
    assert_eq!(
        replay_state, state,
        "replay must return the original terminal state: {replay}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let replay_events = event_count(&base, token, "op-fail-1").await;
    assert_eq!(
        replay_events, first_events,
        "replay must not append new execution events"
    );

    // ② 新操作 ID：不得瞬时 completed——两条合法路径：
    //    a) 第一次 clean Failed → 新受理真实执行（事件序列 + 再次终态）；
    //    b) 第一次 RecoveryRequired → 恢复保护明确拒绝（ERR_RECOVERY_REQUIRED）。
    let fresh_identity = with_revision(&base, token, wait_identity(&base, token).await).await;
    match submit_start(&base, token, &fresh_identity, "op-fail-2").await {
        Ok(second) => {
            let submitted_state = second["data"]["state"].as_str().unwrap_or_default();
            assert_ne!(
                submitted_state, "succeeded",
                "new start must not be instantly completed: {second}"
            );
            let (state2, view2) = poll_terminal(&base, token, "op-fail-2", &logs).await;
            assert_ne!(
                state2, "succeeded",
                "second run must reach a real terminal (fail/recovery), not success: {view2}"
            );
            let second_events = event_count(&base, token, "op-fail-2").await;
            assert!(
                second_events >= 2,
                "second run must have its own execution evidence: {second_events}"
            );
        }
        Err(rejection) => {
            assert_eq!(
                rejection["code"], "ERR_RECOVERY_REQUIRED",
                "rejection after uncertain outcome must be explicit: {rejection}"
            );
        }
    }

    // 清理
    let _ = Command::new("kill")
        .args(["-TERM", &server.0.id().to_string()])
        .status();
    let deadline = Instant::now() + Duration::from_secs(10);
    while matches!(server.0.try_wait(), Ok(None)) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
