//! Real native CLI regression: clean stop must permit the same workspace to run again.
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

fn start(workspace: &Path, logs: &Path) -> (OwnedServer, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve test address");
    let address = listener.local_addr().expect("test address");
    drop(listener);
    let child = Command::new(env!("CARGO_BIN_EXE_app-cli"))
        .args([
            "--workspace",
            workspace.to_str().expect("workspace path"),
            "--log-dir",
            logs.to_str().expect("log path"),
            "--admin-addr",
            &address.to_string(),
            "serve",
        ])
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

async fn expect_phase(server: &mut OwnedServer, base: &str, phase: &str) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "server exited before status became available"
        );
        if let Ok(response) = client.get(format!("{base}/v1/deploy/status")).send().await {
            assert!(response.status().is_success());
            let body: serde_json::Value = response.json().await.unwrap();
            assert_eq!(
                body["data"]["phase"], phase,
                "normal repeated serve must not be blocked by stale active owner: {body}"
            );
            return;
        }
        assert!(Instant::now() < deadline, "owned server API did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop_cleanly(server: &mut OwnedServer) {
    let status = Command::new("kill")
        .args(["-TERM", &server.0.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "send SIGTERM to owned test process");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = server.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "clean shutdown returned failure: {status}"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "owned server did not stop cleanly"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn empty_native_serve_can_restart_after_clean_sigterm() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("code");
    let logs = root.path().join("logs");
    let (mut first, first_url) = start(&workspace, &logs);
    expect_phase(&mut first, &first_url, "idle").await;
    stop_cleanly(&mut first).await;
    let (mut second, second_url) = start(&workspace, &logs);
    expect_phase(&mut second, &second_url, "idle").await;
    stop_cleanly(&mut second).await;
}

#[tokio::test]
async fn rejected_native_serve_sigterm_does_not_clear_previous_active_owner() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("code");
    let logs = root.path().join("logs");
    let (mut first, first_url) = start(&workspace, &logs);
    expect_phase(&mut first, &first_url, "idle").await;
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    let owner = root.path().join(".deploy-coordinator.json");
    let before = std::fs::read(&owner).unwrap();
    let (mut rejected, url) = start(&workspace, &logs);
    expect_phase(&mut rejected, &url, "orchestrating").await;
    assert!(
        Command::new("kill")
            .args(["-TERM", &rejected.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = rejected.0.try_wait().unwrap() {
            assert!(
                !status.success(),
                "unclaimed coordinator must report failed startup"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rejected server did not terminate"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(std::fs::read(&owner).unwrap(), before);
    let (mut third, third_url) = start(&workspace, &logs);
    expect_phase(&mut third, &third_url, "orchestrating").await;
}
