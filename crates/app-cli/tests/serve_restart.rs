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
    start_with_env(workspace, logs, &[])
}

fn start_with_env(
    workspace: &Path,
    logs: &Path,
    envs: &[(String, String)],
) -> (OwnedServer, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve test address");
    let address = listener.local_addr().expect("test address");
    drop(listener);
    let mut command = Command::new(env!("CARGO_BIN_EXE_app-cli"));
    command
        .args([
            "serve",
            "--workspace",
            workspace.to_str().expect("workspace path"),
            "--log-dir",
            logs.to_str().expect("log path"),
            "--admin-addr",
            &address.to_string(),
        ])
        .env_remove("APP_DEPLOY_URL")
        .env_remove("APP_RELEASE_ID")
        .env_remove("APP_DEPLOY_OPERATION_ID")
        .env_remove("APP_DEPLOY_GENERATION_ID")
        .env_remove("APP_DEPLOY_SHA256")
        // N01 后空 workspace 编排不再被 60s PG 等待拖住——显式声明 PG 前置
        // 恢复确定性"orchestrating"观察窗口（本机无 PG，探测满窗失败）
        .env("APP_CLI_REQUIRE_PG", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in envs {
        command.env(key, value);
    }
    let child = command.spawn().expect("spawn owned app-cli");
    (OwnedServer(child), format!("http://{address}"))
}

/// 模拟平台容器身份（K8s pod UID 等）：同一 authority/volume 下不同 instance
/// 即不同容器。
fn domain_envs(instance: &str) -> Vec<(String, String)> {
    vec![
        (
            "RCODER_EXECUTION_DOMAIN".into(),
            serde_json::json!({
                "authority": "serve-restart-test",
                "volume": "workspace",
                "instance": "",
                "instance_source_env": "APP_TEST_CONTAINER_INSTANCE",
            })
            .to_string(),
        ),
        ("APP_TEST_CONTAINER_INSTANCE".into(), instance.into()),
    ]
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
            if body["data"]["phase"] == phase {
                return;
            }
            // The management listener is bound before startup recovery begins.
            // Its initial Idle response is not the completed startup result.
            assert_eq!(
                body["data"]["phase"], "idle",
                "unexpected startup state: {body}"
            );
            assert!(
                Instant::now() < deadline,
                "server never reached {phase}: {body}"
            );
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
async fn native_serve_recovers_confirmed_generation_after_supervisor_crash() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("code");
    let logs = root.path().join("logs");
    let (mut first, first_url) = start(&workspace, &logs);
    expect_phase(&mut first, &first_url, "idle").await;
    let scope = runtime_state_layout::ensure_state_root(&workspace, None, None).unwrap();
    let before = runtime_supervisor::last_snapshot(&scope).unwrap();
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    // The old test expected all crash restarts to remain blocked. A supervised
    // generation now has positive cleanup evidence; the successor may recover.
    let (mut second, second_url) = start(&workspace, &logs);
    expect_phase(&mut second, &second_url, "idle").await;
    let old_generation = before.generation.as_deref().unwrap();
    runtime_supervisor::verify_quiescent(&scope, old_generation).unwrap();
    let after = runtime_supervisor::last_snapshot(&scope).unwrap();
    assert_ne!(before.generation, after.generation);
    assert_ne!(before.supervisor_id, after.supervisor_id);
    stop_cleanly(&mut second).await;
}

#[tokio::test]
async fn replaced_container_consumes_deploy_declaration_env() {
    // 事故回归（2026-09-28，nuwax-k8s-test app 197）：PVC 复用 + Pod 换代后，
    // 上一容器死在 supervisor ready 态；新容器首次 worker 启动不得按"恢复式
    // 重启"剥离部署声明 env。APP_DEPLOY_URL 指向不可达地址 → 部署被真实受理
    // 并诚实 Failed；修复前则是永久 idle、operation=null（rcoder 侧 30 分钟
    // 僵尸等待 + 后续 start 全部 ERR_CONFLICT）。
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("code");
    let logs = root.path().join("logs");

    let (mut first, first_url) = start_with_env(&workspace, &logs, &domain_envs("container-a"));
    expect_phase(&mut first, &first_url, "idle").await;
    let scope = runtime_state_layout::ensure_state_root(&workspace, None, None).unwrap();
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = runtime_supervisor::last_snapshot(&scope).unwrap();
        if snapshot.phase == runtime_supervisor::Phase::Ready {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "first container never reached ready: {snapshot:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // 容器消亡：SIGKILL，无优雅退出记录，supervisor.json 停留 ready。
    first.0.kill().unwrap();
    first.0.wait().unwrap();

    let operation_id = "replaced-deploy-op-1";
    let mut envs = domain_envs("container-b");
    envs.push((
        "APP_DEPLOY_URL".into(),
        "http://127.0.0.1:1/absent.zip".into(),
    ));
    envs.push(("APP_RELEASE_ID".into(), "rel-replaced-1".into()));
    envs.push(("APP_DEPLOY_OPERATION_ID".into(), operation_id.into()));
    envs.push(("APP_DEPLOY_GENERATION_ID".into(), operation_id.into()));
    // 不可达上游按生产语义会在重试预算内反复重试；测试收紧到 2s 收敛失败。
    envs.push(("APP_DEPLOY_UNAVAILABLE_RETRY_SECONDS".into(), "2".into()));
    let (mut second, second_url) = start_with_env(&workspace, &logs, &envs);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let mut latest = serde_json::Value::Null;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            second.0.try_wait().unwrap().is_none(),
            "replacement server exited before the declaration was consumed"
        );
        if let Ok(response) = client
            .get(format!("{second_url}/v1/deploy/status"))
            .send()
            .await
        {
            assert!(response.status().is_success());
            let body: serde_json::Value = response.json().await.unwrap();
            latest = body.clone();
            if body["data"]["operation"]["operation_id"] == operation_id {
                break;
            }
            let phase = body["data"]["phase"].as_str().unwrap_or_default();
            assert!(
                phase == "idle" || phase == "deploying",
                "unexpected phase before declaration was consumed: {body}"
            );
        }
        assert!(
            Instant::now() < deadline,
            "deploy declaration was never consumed by the replacement container: {latest}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 声明已被受理：不可达制品必须收敛为诚实 Failed（而非无限 Pending）。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = client
            .get(format!("{second_url}/v1/deploy/status"))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = response.json().await.unwrap();
        if body["data"]["phase"] == "failed"
            && body["data"]["operation"]["operation_id"] == operation_id
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unreachable artifact must fail the deployment honestly: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop_cleanly(&mut second).await;
}
