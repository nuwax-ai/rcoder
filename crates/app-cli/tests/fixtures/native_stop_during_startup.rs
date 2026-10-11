//! Actual run client, native owner, generation control and retained Child chain.
//! Proxy HTTP is a controlled protocol fixture, not official Pingap acceptance.
use super::*;
use std::{os::unix::fs::PermissionsExt as _, path::Path};

struct StartupHold(PathBuf);

impl Drop for StartupHold {
    fn drop(&mut self) {
        let _result = std::fs::remove_dir(&self.0);
    }
}

fn quote(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "'\"'\"'")
}

async fn entered(fixture: &Fixture, path: &Path, standby: bool) -> u32 {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(record) = std::fs::read_to_string(path) {
                let mut lines = record.lines();
                let pid = lines.next().unwrap().parse::<u32>().unwrap();
                let config = PathBuf::from(lines.next().unwrap());
                let config =
                    pingap_config::PingapConfig::new(&std::fs::read(config).unwrap(), true)
                        .unwrap();
                assert!(config.plugins.contains_key("rcoder:publication"));
                assert_eq!(config.locations.contains_key("standby"), standby);
                assert!(process_utils::process_exists(pid).unwrap());
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "actual proxy barrier missing: {error}; {}",
            fixture.diagnostics()
        )
    })
}

#[tokio::test]
async fn native_stop_during_source_proxy_start_retains_its_drain_and_fresh_source_recovers() {
    let mut fixture = Fixture::new();
    let hold_path = fixture.source.join("startup-proxy-hold");
    let normal_seen = fixture.source.join("startup-normal-entered");
    let standby_seen = fixture.source.join("startup-standby-entered");
    let business_pid_file = fixture.source.join("web/startup-business.pid");
    let sentinel = fixture.source.join("startup-sentinel");
    std::fs::write(&sentinel, "preserved original workspace data").unwrap();

    // Establish an actual predecessor and an authoritative Stop before the
    // target Source operation. A cold owner with no confirmed proxy may legally
    // drain absence without launching standby; this scenario exercises an
    // existing verified standby. It never waits for the target Start to succeed.
    assert!(
        fixture
            .command("gen-lock", "predecessor-lock")
            .status()
            .unwrap()
            .success()
    );
    let predecessor_client = fixture.spawn_run("predecessor-run");
    let client = client();
    let (base, identity, native) = fixture.ready(&client).await;
    fixture.http(&client, "first").await;
    assert!(
        operation_client_exit(&mut fixture, predecessor_client)
            .await
            .success()
    );
    fixture.operation(&client, &base, &identity, "stop").await;
    let token = std::fs::read_to_string(fixture.state.join("token")).unwrap();
    let proxy: Value = client
        .get(format!("{base}/v1/proxy/status"))
        .header("X-Deploy-Token", token.trim())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(proxy["data"]["confirmation"]["confirmed"], true, "{proxy}");
    assert_eq!(proxy["data"]["confirmation"]["phase"], "standby", "{proxy}");
    fixture.same_owner(&client, &base, &identity, &native).await;

    // Hold only the target normal publication. A verified standby is real
    // predecessor evidence, not a fabricated target Start success.
    let helper = std::env::var_os("APP_CLI_TEST_TREE_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_tree-fixture").into());
    std::fs::write(
        &fixture.pingap,
        format!(
            "#!/bin/sh\n\
             if [ \"$1\" = '-c' ] && grep -q 'rcoder:publication' \"$2\"; then\n\
               startup_seen='{normal_seen}'\n\
               if grep -q '^\\[locations.standby\\]' \"$2\"; then\n\
                 startup_seen='{standby_seen}'\n\
                 exec '{helper}' protocol-pingap \"$@\"\n\
               fi\n\
               if [ -d '{hold}' ]; then\n\
                 startup_pending=\"$startup_seen\".\"$$\".pending\n\
                 printf '%s\\n%s\\n' \"$$\" \"$2\" > \"$startup_pending\" || exit 1\n\
                 mv \"$startup_pending\" \"$startup_seen\" || exit 1\n\
                 while [ -d '{hold}' ]; do sleep 0.03; done\n\
               fi\n\
             fi\n\
             exec '{helper}' protocol-pingap \"$@\"\n",
            normal_seen = quote(&normal_seen),
            standby_seen = quote(&standby_seen),
            hold = quote(&hold_path),
            helper = quote(Path::new(&helper)),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture.pingap, std::fs::Permissions::from_mode(0o755)).unwrap();
    let command = toml::Value::Array(
        [
            "sh".to_owned(),
            "-c".to_owned(),
            format!(
                "startup_pending='{}'.\"$$\".pending; printf '%s' \"$$\" > \"$startup_pending\" && mv \"$startup_pending\" '{}' && exec '{}' http first",
                quote(&business_pid_file),
                quote(&business_pid_file),
                quote(Path::new(&helper)),
            ),
        ]
        .into_iter()
        .map(toml::Value::String)
        .collect(),
    );
    fixture.write_manifests(command, "native Stop during cold proxy publication", 5);
    assert!(
        fixture
            .command("gen-lock", "startup-stop-lock")
            .status()
            .unwrap()
            .success(),
        "{}",
        fixture.diagnostics()
    );
    std::fs::create_dir(&hold_path).unwrap();
    let hold = StartupHold(hold_path);
    let original_client = fixture.spawn_run("startup-stop-original-run");
    let normal_pid = entered(&fixture, &normal_seen, false).await;
    let business_port = fixture.http(&client, "first").await;
    let business_pid: u32 = std::fs::read_to_string(&business_pid_file)
        .unwrap()
        .parse()
        .unwrap();
    assert!(process_utils::process_exists(business_pid).unwrap());
    let start_id = await_active_operation(&fixture, &client, &base).await;
    let token = std::fs::read_to_string(fixture.state.join("token")).unwrap();
    let initial_start = operation_view(&client, &base, &token, &start_id).await;
    assert!(matches!(
        initial_start["state"].as_str(),
        Some("accepted" | "running")
    ));
    assert_eq!(
        initial_start["runtime_instance_id"],
        identity["runtime_instance_id"]
    );

    let executing = runtime_supervisor::control_verified(
        &fixture.state,
        runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        &native.supervisor_id,
    )
    .await
    .unwrap();
    let stopped_generation = executing.generation.as_deref().unwrap().to_owned();
    let mut stop = runtime_supervisor::Request::new(runtime_supervisor::Action::StopWork);
    stop.capture_generation(Some(&stopped_generation));
    let admitted =
        runtime_supervisor::control_verified(&fixture.state, stop.clone(), &native.supervisor_id)
            .await
            .unwrap();
    assert_eq!(
        admitted.operation_id.as_deref(),
        Some(stop.request_id.as_str())
    );
    assert!(matches!(
        admitted.phase,
        runtime_supervisor::Phase::Stopping | runtime_supervisor::Phase::CleanupPending
    ));

    // The captured Stop must reap the actual in-flight publication. A cleanup
    // confirmed as empty may drain absence instead of spawning another proxy.
    tokio::time::timeout(Duration::from_secs(10), async {
        while process_utils::process_exists(normal_pid).unwrap() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the exact in-flight proxy must be physically reaped by Stop");
    // A revoked candidate whose cleanup is confirmed may leave no proxy.
    // Stop can prove entry absence; it need not launch an extra standby. The
    // captured native request must still finish successfully and retain its
    // owner. The actor-level test separately holds the exact drain barrier.
    drop(hold);

    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let result = runtime_supervisor::control_verified(
                &fixture.state,
                stop.clone(),
                &native.supervisor_id,
            )
            .await
            .unwrap();
            assert_eq!(
                result.operation_id.as_deref(),
                Some(stop.request_id.as_str())
            );
            if result.phase == runtime_supervisor::Phase::Stopped {
                assert_eq!(result.intent, runtime_supervisor::Intent::Stopped);
                break;
            }
            assert!(
                !matches!(result.phase, runtime_supervisor::Phase::RecoveryRequired),
                "the original native Stop failed: {result:?}; {}",
                fixture.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "original native Stop did not settle: {error}; {}",
            fixture.diagnostics()
        )
    });
    let receipt =
        runtime_supervisor::verify_quiescent(&fixture.state, &stopped_generation).unwrap();
    assert_eq!(receipt.generation, stopped_generation);
    assert_eq!(receipt.supervisor_id, native.supervisor_id);
    assert!(!process_utils::process_exists(normal_pid).unwrap());
    assert!(!process_utils::process_exists(business_pid).unwrap());
    assert!(
        client
            .get(format!("http://127.0.0.1:{business_port}/"))
            .send()
            .await
            .is_err()
    );
    // Native Stop has a physical terminal before the new management session
    // reopens and reconciles the interrupted kernel operation. Observe that
    // same original ID to a bounded known failure; no transient recovery view
    // or a late success is accepted as completion.
    let original_start = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let view = operation_view(&client, &base, &token, &start_id).await;
            assert_ne!(
                view["state"], "succeeded",
                "interrupted Source reported late success"
            );
            if matches!(view["state"].as_str(), Some("failed" | "cancelled")) {
                break view;
            }
            assert!(
                matches!(
                    view["state"].as_str(),
                    Some("accepted" | "running" | "recovery_required")
                ),
                "unexpected original state: {view}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("original Source must converge to known failure after physical Stop");
    assert_eq!(original_start["operation_id"], start_id);
    assert_eq!(
        original_start["runtime_instance_id"],
        identity["runtime_instance_id"]
    );
    assert!(
        matches!(
            original_start["state"].as_str(),
            Some("failed" | "cancelled")
        ),
        "interrupted Source must have a known non-success terminal: {original_start}"
    );
    assert!(
        !operation_client_exit(&mut fixture, original_client)
            .await
            .success()
    );
    fixture.same_owner(&client, &base, &identity, &native).await;
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "preserved original workspace data"
    );

    fixture.manifests("corrected-after-native-stop");
    fixture.operation(&client, &base, &identity, "start").await;
    fixture.http(&client, "corrected-after-native-stop").await;
    fixture.same_owner(&client, &base, &identity, &native).await;
    let original_after_fresh_start = operation_view(&client, &base, &token, &start_id).await;
    assert_eq!(
        original_after_fresh_start["state"], original_start["state"],
        "new Start must not rewrite the old terminal"
    );
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "preserved original workspace data"
    );
}
