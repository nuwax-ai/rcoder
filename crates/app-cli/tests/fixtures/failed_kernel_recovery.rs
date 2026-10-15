//! A real failed serve retains its lock; run must retire it through native control.
use super::*;

#[tokio::test]
async fn run_recovers_readable_but_invalid_control_token_without_manual_cleanup() {
    let mut fixture = Fixture::new();
    let first = fixture.spawn_run("original-token");
    assert!(
        operation_client_exit(&mut fixture, first).await.success(),
        "{}",
        fixture.diagnostics()
    );
    let client = client();
    let (_, _, original) = fixture.ready(&client).await;
    fixture.http(&client, "first").await;
    std::fs::write(fixture.state.join("token"), b"corrupted-control-token").unwrap();
    fixture.manifests("after-token-repair");
    let retry = fixture.spawn_run("token-repair");
    assert!(
        operation_client_exit(&mut fixture, retry).await.success(),
        "{}",
        fixture.diagnostics()
    );
    let (_, _, successor) = fixture.ready(&client).await;
    assert_ne!(original.supervisor_id, successor.supervisor_id);
    fixture.http(&client, "after-token-repair").await;
    assert!(fixture.state.join("owner.lock").exists());
    assert!(
        runtime_supervisor::Owner::try_acquire(&fixture.state)
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn run_rebuilds_same_scope_manager_after_its_identity_record_is_damaged() {
    let mut fixture = Fixture::new();
    let first = fixture.spawn_run("original-record");
    assert!(
        operation_client_exit(&mut fixture, first).await.success(),
        "{}",
        fixture.diagnostics()
    );
    let client = client();
    let (_, _, original) = fixture.ready(&client).await;
    fixture.http(&client, "first").await;
    std::fs::write(fixture.state.join("identity.json"), b"{damaged identity").unwrap();
    fixture.manifests("after-record-repair");
    let retry = fixture.spawn_run("record-repair");
    assert!(
        operation_client_exit(&mut fixture, retry).await.success(),
        "{}",
        fixture.diagnostics()
    );
    let (_, _, successor) = fixture.ready(&client).await;
    assert_ne!(original.supervisor_id, successor.supervisor_id);
    fixture.http(&client, "after-record-repair").await;
    assert!(fixture.state.join("owner.lock").exists());
    assert!(
        runtime_supervisor::Owner::try_acquire(&fixture.state)
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_rebuilds_failed_kernel_after_storage_problem_is_fixed() {
    let mut fixture = Fixture::new();
    std::fs::create_dir_all(fixture.state.join("token")).unwrap();
    let port = protocol_support::reserve_port();
    let address = format!("127.0.0.1:{port}");
    let mut owner = fixture.command("serve", "failed-manager");
    owner
        .args(["--control-only", "--admin-addr", &address, "--log-dir"])
        .arg(fixture.source.join("logs"))
        .arg("--pingap-bin")
        .arg(&fixture.pingap);
    fixture.clients.push(owner.spawn().unwrap());
    let owner_index = fixture.clients.len() - 1;
    let captured_pid = fixture.clients[owner_index].id();
    let client = reqwest::Client::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(reply) = client
                .get(format!("http://{address}/v1/runtime/identity"))
                .send()
                .await
                && reply.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE
                && let Ok(body) = reply.json::<Value>().await
                && body["code"] == "ERR_RECOVERY_REQUIRED"
            {
                break;
            }
            assert!(
                fixture.clients[owner_index].try_wait().unwrap().is_none(),
                "failed manager must stay alive: {}",
                fixture.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("observe failed kernel");
    assert!(
        runtime_supervisor::Owner::try_acquire(&fixture.state)
            .unwrap()
            .is_none()
    );
    let native = runtime_supervisor::control(
        &fixture.state,
        runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
    )
    .await
    .unwrap();
    fixture.owner = Some(native.supervisor_id.clone());
    fixture.owner_pid = Some(captured_pid);
    // Correct the actual storage fault, retaining every lock and history file.
    std::fs::remove_dir(fixture.state.join("token")).unwrap();
    fixture.manifests("repaired-manager-source");
    let run = fixture.command("run", "rebuild-manager").spawn().unwrap();
    fixture.clients.push(run);
    let run_index = fixture.clients.len() - 1;
    tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            if let Some(status) = fixture.clients[run_index].try_wait().unwrap() {
                assert!(status.success(), "{}", fixture.diagnostics());
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "run did not rebuild failed manager: {}",
            fixture.diagnostics()
        )
    });
    let (_, _, successor) = fixture.ready(&client).await;
    assert_ne!(successor.supervisor_id, native.supervisor_id);
    assert!(fixture.clients[owner_index].try_wait().unwrap().is_some());
    fixture.http(&client, "repaired-manager-source").await;
    assert!(fixture.state.join("owner.lock").exists());
    assert!(
        runtime_supervisor::Owner::try_acquire(&fixture.state)
            .unwrap()
            .is_none()
    );
}
