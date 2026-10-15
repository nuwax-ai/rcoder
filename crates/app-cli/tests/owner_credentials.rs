//! Real owner/API contract; no Pingap or business services are started.
use serde_json::{Value, json};
use std::{
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Owner(Child);
impl Drop for Owner {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[tokio::test]
async fn owner_publishes_usable_credentials_without_environment_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let state = temp.path().join("state");
    std::fs::create_dir(&workspace).unwrap();
    let mut owner = Owner(
        Command::new(env!("CARGO_BIN_EXE_app-cli"))
            .arg("serve")
            .arg("--workspace")
            .arg(&workspace)
            .arg("--log-dir")
            .arg(temp.path().join("logs"))
            .arg("--admin-addr")
            .arg("127.0.0.1:0")
            .env("PROJECT_ID", "nativecredentialtest")
            .env("APP_CLI_STATE_ROOT", &state)
            .env_remove("APP_CLI_DEPLOY_TOKEN")
            .env_remove("APP_DEPLOY_URL")
            .env_remove("APP_CLI_ATTACH")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let endpoint = loop {
        assert!(
            owner.0.try_wait().unwrap().is_none(),
            "owner exited before endpoint publication"
        );
        if let Ok(bytes) = std::fs::read(state.join("endpoint.json")) {
            break serde_json::from_slice::<Value>(&bytes).unwrap();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "owner did not publish its listener"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    let address: std::net::SocketAddr = endpoint["address"].as_str().unwrap().parse().unwrap();
    assert_ne!(
        address.port(),
        0,
        "discovery must publish the actual bound port"
    );
    let base = format!("http://{address}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let identity = loop {
        assert!(
            owner.0.try_wait().unwrap().is_none(),
            "owner exited during initialization"
        );
        if let Ok(response) = client
            .get(format!("{base}/v1/runtime/identity"))
            .send()
            .await
            && response.status().is_success()
            && let Ok(body) = response.json::<Value>().await
            && body["data"].is_object()
            && state.join("token").is_file()
            && let Ok(ready) = client.get(format!("{base}/ready")).send().await
            && ready.status().is_success()
        {
            break body["data"].clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "owner did not publish identity/credential"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    let token = std::fs::read_to_string(state.join("token")).unwrap();
    assert_eq!(
        endpoint["runtime_instance_id"],
        identity["runtime_instance_id"]
    );
    assert!(!token.trim().is_empty());
    let status: Value = client
        .get(format!("{base}/v1/runtime/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let request = json!({
        "operation_id":"credentialstop",
        "expected_runtime_instance_id":identity["runtime_instance_id"],
        "expected_revision":status["data"]["revision"],
        "workspace_id":identity["workspace_id"],
        "kind":"stop",
        "profile":{"profile":"source","input":{"workspace_id":identity["workspace_id"]}}
    });
    let url = format!("{base}/v1/runtime/operations");
    let denied = client
        .post(&url)
        .header("X-Deploy-Token", "wrong-token")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    let response = client
        .post(&url)
        .header("X-Deploy-Token", token.trim())
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let view: Value = client
            .get(format!("{url}/credentialstop"))
            .header("X-Deploy-Token", token.trim())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let result = &view["data"];
        assert_eq!(result["operation_id"], "credentialstop");
        assert_eq!(
            result["runtime_instance_id"],
            identity["runtime_instance_id"]
        );
        if result["state"] == "succeeded" {
            break;
        }
        assert!(
            result["state"] == "accepted" || result["state"] == "running",
            "unexpected terminal state: {}",
            result["state"]
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "stop did not complete"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let status: Value = client
        .get(format!("{base}/v1/runtime/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["data"]["desired"], "stopped");
}
