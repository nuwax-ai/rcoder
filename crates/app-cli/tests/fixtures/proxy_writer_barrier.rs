//! Real owner + router + durable storage barrier; proxy routing is a controlled
//! protocol fixture, not official Pingap/AI acceptance.
use super::*;
use std::os::unix::fs::PermissionsExt as _;

struct Hold(PathBuf);
impl Drop for Hold {
    fn drop(&mut self) {
        let _result = std::fs::remove_dir(&self.0);
    }
}

async fn status(client: &reqwest::Client, base: &str) -> Value {
    client
        .get(format!("{base}/v1/runtime/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn submit(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    identity: &Value,
    revision: &Value,
    kind: &str,
    id: &str,
) -> reqwest::Response {
    let profile = if kind == "deploy" {
        json!({"profile":"artifact","input":{"artifact":{"source":"url","value":{"url":"https://fixture.invalid/app.zip","sha256":null}}}})
    } else {
        json!({"profile":"source","input":{"workspace_id":identity["workspace_id"]}})
    };
    client.post(format!("{base}/v1/runtime/operations")).header("X-Deploy-Token", token)
        .json(&json!({"operation_id":id,"expected_runtime_instance_id":identity["runtime_instance_id"],"expected_revision":revision,"workspace_id":identity["workspace_id"],"kind":kind,"profile":profile}))
        .send().await.unwrap()
}

async fn wait_terminal(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    id: &str,
    expected: &str,
) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let view: Value = client
                .get(format!("{base}/v1/runtime/operations/{id}"))
                .header("X-Deploy-Token", token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if view["data"]["state"] == expected {
                break;
            }
            assert!(
                matches!(view["data"]["state"].as_str(), Some("accepted" | "running")),
                "unexpected terminal: {view}"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reload_preflight_fences_runtime_and_stop_waits_for_the_owned_writer() {
    for stopped in [false, true] {
        for dispatched in [false, true] {
            run_barrier(false, dispatched, stopped).await;
        }
    }
}

#[tokio::test]
async fn detached_reload_shutdown_keeps_the_owner_lock_until_preflight_is_settled() {
    for stopped in [false, true] {
        for dispatched in [false, true] {
            run_barrier(true, dispatched, stopped).await;
        }
    }
}

async fn run_barrier(shutdown: bool, dispatched: bool, stopped: bool) {
    let mut fixture = Fixture::new();
    let hold = fixture.source.join("native-test-hold");
    let seen = fixture.source.join("native-test-entered");
    let helper = std::env::var_os("APP_CLI_TEST_TREE_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_tree-fixture").into());
    let quote = |path: &std::path::Path| path.to_string_lossy().replace('\'', "'\"'\"'");
    let condition = if dispatched {
        "[ \"$1\" = '-c' ] && grep -q 'rcoder:publication' \"$2\""
    } else {
        "[ \"$1\" = '-t' ]"
    };
    // Publish only the complete PID. Creating the visible file with shell
    // redirection lets the reader observe an empty file before printf writes.
    let pending = fixture.source.join("native-test-entered.pending");
    std::fs::write(
        &fixture.pingap,
        format!(
            "#!/bin/sh\nif {condition} && [ -d '{hold}' ]; then\n\
             native_test_pending='{pending}'.\"$$\"\n\
             printf '%s' \"$$\" > \"$native_test_pending\" || exit 1\n\
             mv \"$native_test_pending\" '{seen}' || exit 1\n\
             while [ -d '{hold}' ]; do sleep 0.03; done\n\
             fi\nexec '{helper}' protocol-pingap \"$@\"\n",
            hold = quote(&hold),
            pending = quote(&pending),
            seen = quote(&seen),
            helper = quote(std::path::Path::new(&helper)),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture.pingap, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        fixture
            .command("gen-lock", "barrier-lock")
            .status()
            .unwrap()
            .success()
    );
    fixture.spawn_run("barrier-run");
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    let business_port = fixture.http(&client, "first").await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let view = status(&client, &base).await;
            if view["data"]["active_operation_id"].is_null() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let token = std::fs::read_to_string(fixture.state.join("token")).unwrap();
    let token = token.trim().to_string();
    if stopped {
        let revision = status(&client, &base).await["data"]["revision"].clone();
        assert_eq!(
            submit(
                &client,
                &base,
                &token,
                &identity,
                &revision,
                "stop",
                "initial-stop"
            )
            .await
            .status(),
            reqwest::StatusCode::ACCEPTED
        );
        wait_terminal(&client, &base, &token, "initial-stop", "succeeded").await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let readiness: Value = client
                    .get(format!("{base}/ready"))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if readiness["phase"] == "idle" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("confirmed Stop must converge to Idle before the reload barrier");
        assert!(
            client
                .get(format!("http://127.0.0.1:{business_port}/"))
                .send()
                .await
                .is_err(),
            "Stopped setup must confirm the actual business listener is gone"
        );
    }
    std::fs::create_dir(&hold).unwrap();
    let barrier = Hold(hold);
    let long_client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let request_url = format!("{base}/v1/proxy/reload");
    let request_token = token.clone();
    let reload = tokio::spawn(async move {
        long_client
            .post(request_url)
            .header("X-Deploy-Token", request_token)
            .header("X-Request-ID", "barrier-reload")
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(15), async {
        while !seen.exists() {
            assert!(
                !reload.is_finished(),
                "reload ended before real -t barrier: {}",
                fixture.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let blocked_pid: u32 = std::fs::read_to_string(&seen).unwrap().parse().unwrap();
    let revision = status(&client, &base).await["data"]["revision"].clone();
    for kind in ["start", "restart", "deploy"] {
        let id = format!("rejected-{kind}");
        let response = submit(&client, &base, &token, &identity, &revision, kind, &id).await;
        let code = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            code,
            reqwest::StatusCode::CONFLICT,
            "reload writer must fence {kind} before admission: {body}"
        );
        assert_eq!(body["code"], "ERR_OPERATION_IN_PROGRESS", "{body}");
        assert_eq!(body["active_operation_id"], "barrier-reload");
        assert_eq!(
            client
                .get(format!("{base}/v1/runtime/operations/{id}"))
                .header("X-Deploy-Token", &token)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }
    assert_eq!(status(&client, &base).await["data"]["revision"], revision);
    if shutdown {
        reload.abort();
        assert!(reload.await.unwrap_err().is_cancelled());
        let mut request = runtime_supervisor::Request::new(runtime_supervisor::Action::Shutdown);
        request.expected_generation = native.generation.clone();
        let control =
            runtime_supervisor::control_verified(&fixture.state, request, &native.supervisor_id)
                .await
                .unwrap();
        assert!(matches!(
            control.phase,
            runtime_supervisor::Phase::Stopping | runtime_supervisor::Phase::CleanupPending
        ));
        let held = runtime_supervisor::Owner::try_acquire(&fixture.state)
            .unwrap()
            .is_none();
        assert!(
            held || (dispatched && !process_utils::process_exists(blocked_pid).unwrap()),
            "shutdown can release owner only after the exact dispatched process is physically gone"
        );
        drop(barrier);
        tokio::time::timeout(Duration::from_secs(30), async {
            while runtime_supervisor::Owner::try_acquire(&fixture.state)
                .unwrap()
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        assert!(std::net::TcpStream::connect("127.0.0.1:9080").is_err());
    } else {
        assert_eq!(
            submit(
                &client,
                &base,
                &token,
                &identity,
                &revision,
                "stop",
                "priority-stop"
            )
            .await
            .status(),
            reqwest::StatusCode::ACCEPTED
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let view: Value = client
            .get(format!("{base}/v1/runtime/operations/priority-stop"))
            .header("X-Deploy-Token", &token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if !dispatched {
            assert_ne!(
                view["data"]["state"], "succeeded",
                "Stop cannot declare cleanup while native preflight is still blocked"
            );
        } else if view["data"]["state"] == "succeeded" {
            assert!(
                !process_utils::process_exists(blocked_pid).unwrap(),
                "early Stop success requires exact dispatched process cleanup"
            );
        }
        drop(barrier);
        let outcome = tokio::time::timeout(Duration::from_secs(20), reload)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            outcome["success"], false,
            "cancelled reload cannot publish success: {outcome}"
        );
        wait_terminal(&client, &base, &token, "priority-stop", "succeeded").await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        for kind in ["start", "restart", "deploy"] {
            assert_eq!(
                client
                    .get(format!("{base}/v1/runtime/operations/rejected-{kind}"))
                    .header("X-Deploy-Token", &token)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                reqwest::StatusCode::NOT_FOUND
            );
        }
        assert!(!status(&client, &base).await["data"]["active_operation_id"].is_string());
    }
}
