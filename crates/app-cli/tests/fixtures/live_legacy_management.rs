//! Actual owners and business child trees across the old/new state-root domains.
//! The parent integration target's routing binary is a protocol stand-in.
use super::*;

struct NativeRootCleanup {
    root: PathBuf,
    workspace: PathBuf,
}
impl NativeRootCleanup {
    fn new(root: PathBuf, workspace: PathBuf) -> Self {
        Self { root, workspace }
    }
}
impl Drop for NativeRootCleanup {
    fn drop(&mut self) {
        let root = self.root.clone();
        let workspace = self.workspace.clone();
        let cleanup = std::thread::spawn(move || {
            runtime_supervisor::runtime().unwrap().block_on(async {
                if let Ok(snapshot) = runtime_supervisor::control(
                    &root,
                    runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                )
                .await
                    && snapshot.binding.component == "app-cli"
                    && runtime_state_layout::canonical_project_root(&snapshot.binding.resource)
                        == runtime_state_layout::canonical_project_root(&workspace)
                {
                    let mut request =
                        runtime_supervisor::Request::new(runtime_supervisor::Action::Shutdown);
                    request.capture_generation(snapshot.generation.as_deref());
                    let _result = runtime_supervisor::shutdown_captured_owner(
                        &root,
                        &snapshot,
                        &request,
                        Duration::from_secs(10),
                    )
                    .await;
                }
            });
        });
        let _result = cleanup.join();
    }
}

#[tokio::test]
async fn live_same_app_legacy_owner_is_retired_before_reusing_its_public_port() {
    use std::os::unix::fs::MetadataExt as _;
    let mut fixture = Fixture::new();
    let current_root = fixture.state.clone();
    let legacy_root = fixture.source.join(".app-cli-state");
    let _old_cleanup = NativeRootCleanup::new(legacy_root.clone(), fixture.source.clone());
    let _new_cleanup = NativeRootCleanup::new(current_root.clone(), fixture.source.clone());
    fixture.state = legacy_root.clone();
    fixture.manifests("legacy-live-business");
    let original = fixture.spawn_run("legacy-live-run");
    let client = client();
    let (old_base, old_identity, old_native) = fixture.ready(&client).await;
    assert!(
        operation_client_exit(&mut fixture, original)
            .await
            .success(),
        "{}",
        fixture.diagnostics()
    );
    fixture.http(&client, "legacy-live-business").await;
    let old_owner_pid = fixture.owner_pid.unwrap();
    let generation = old_native
        .generation
        .clone()
        .expect("actual legacy business generation");
    let lock_inode = std::fs::metadata(legacy_root.join("owner.lock"))
        .unwrap()
        .ino();
    assert!(
        runtime_supervisor::Owner::try_acquire(&legacy_root)
            .unwrap()
            .is_none()
    );
    let sentinel = fixture.source.join("legacy-data-sentinel");
    std::fs::write(&sentinel, b"keep legacy data").unwrap();
    fixture.state = current_root;
    fixture.owner = None;
    fixture.owner_pid = None;
    // Exact same public listener exposes an incorrectly placed post-bind
    // recovery hook: the new manager cannot bind while the legacy one is live.
    fixture.admin_addr = old_base.strip_prefix("http://").unwrap().to_owned();
    fixture.manifests("current-source-after-legacy");
    let current = fixture.spawn_run("current-after-legacy");
    let (base, identity, native) = fixture.ready(&client).await;
    assert!(
        operation_client_exit(&mut fixture, current).await.success(),
        "{}",
        fixture.diagnostics()
    );
    fixture.http(&client, "current-source-after-legacy").await;
    assert_eq!(
        base, old_base,
        "the original public port must be usable again"
    );
    assert_ne!(
        identity["runtime_instance_id"],
        old_identity["runtime_instance_id"]
    );
    assert_ne!(native.supervisor_id, old_native.supervisor_id);
    assert!(
        !process_utils::process_exists(old_owner_pid).unwrap(),
        "captured old owner did not exit"
    );
    let proof = runtime_supervisor::verify_quiescent(&legacy_root, &generation)
        .expect("old business child tree must actually finish cleanup");
    assert_eq!(proof.generation, generation);
    assert_eq!(proof.supervisor_id, old_native.supervisor_id);
    let (_, shutdown) =
        runtime_supervisor::completed_shutdown_for(&legacy_root, &old_native.supervisor_id)
            .unwrap()
            .expect("original Shutdown has a real terminal receipt");
    assert_eq!(shutdown.intent, runtime_supervisor::Intent::Shutdown);
    assert_eq!(shutdown.phase, runtime_supervisor::Phase::Stopped);
    assert_eq!(
        std::fs::metadata(legacy_root.join("owner.lock"))
            .unwrap()
            .ino(),
        lock_inode,
        "legacy lock inode must remain unchanged"
    );
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep legacy data");
    fixture.same_owner(&client, &base, &identity, &native).await;
}

#[tokio::test]
async fn explicitly_foreign_live_legacy_root_is_preserved_while_current_source_runs() {
    use std::os::unix::fs::MetadataExt as _;
    let mut fixture = Fixture::new();
    let mut foreign = Fixture::new();
    let foreign_root = fixture.source.join(".app-cli-state");
    foreign.state = foreign_root.clone();
    let _foreign_cleanup = NativeRootCleanup::new(foreign_root.clone(), foreign.source.clone());
    // Two real application proxies cannot share the platform's mandatory
    // 9080 listener on this native host. Keep a real foreign management owner
    // idle instead: it still holds its original lock and native authority.
    let boot_id = uuid::Uuid::new_v4().to_string();
    let mut command = foreign.command("serve", "foreign-live-management");
    command
        .env(shared_types::APP_CLI_MANAGEMENT_BOOT_ID_ENV, &boot_id)
        .args([
            "--control-only",
            "--admin-addr",
            "127.0.0.1:0",
            "--pingap-bin",
        ])
        .arg(&foreign.pingap)
        .arg("--log-dir")
        .arg(foreign.source.join("logs"));
    foreign.clients.push(command.spawn().unwrap());
    let client = client();
    let (foreign_base, foreign_identity, early_native) = foreign.ready(&client).await;
    let identity_bytes = std::fs::read(foreign_root.join("identity.json")).unwrap();
    let token_bytes = std::fs::read(foreign_root.join("token")).unwrap();
    let foreign_token = std::str::from_utf8(&token_bytes).unwrap().trim();
    // Identity is published before the initial management session consumes its
    // boot intent. Observe that real boundary before taking immutable evidence.
    let (initial_status, foreign_native) = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let status: Value = client
                .get(format!("{foreign_base}/v1/runtime/status"))
                .header("X-Deploy-Token", foreign_token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let desired = std::fs::read(foreign_root.join("desired.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            let native = runtime_supervisor::control_verified(
                &foreign_root,
                runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                &early_native.supervisor_id,
            )
            .await
            .unwrap();
            if status["data"]["desired"] == "stopped"
                && desired.as_ref().is_some_and(|record| {
                    record["management_boot_id"].as_str() == Some(boot_id.as_str())
                        && record["desired"] == "stopped"
                })
                && native.phase == runtime_supervisor::Phase::Ready
            {
                break (status, native);
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "foreign management boot was not durably consumed: {}",
            foreign.diagnostics()
        )
    });
    assert_eq!(initial_status["data"]["desired"], "stopped");
    let desired_bytes = std::fs::read(foreign_root.join("desired.json")).unwrap();
    let lock_inode = std::fs::metadata(foreign_root.join("owner.lock"))
        .unwrap()
        .ino();
    let before = runtime_supervisor::control_verified(
        &foreign_root,
        runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        &foreign_native.supervisor_id,
    )
    .await
    .unwrap();
    fixture.manifests("current-with-foreign-legacy");
    let current = fixture.spawn_run("current-with-foreign-legacy");
    let (base, identity, native) = fixture.ready(&client).await;
    assert!(
        operation_client_exit(&mut fixture, current).await.success(),
        "{}",
        fixture.diagnostics()
    );
    fixture.http(&client, "current-with-foreign-legacy").await;
    fixture.same_owner(&client, &base, &identity, &native).await;
    foreign
        .same_owner(&client, &foreign_base, &foreign_identity, &foreign_native)
        .await;
    let after = runtime_supervisor::control_verified(
        &foreign_root,
        runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        &foreign_native.supervisor_id,
    )
    .await
    .unwrap();
    assert_eq!(after.intent, before.intent);
    assert_eq!(after.generation, before.generation);
    assert_eq!(after.supervisor_id, before.supervisor_id);
    let final_status: Value = client
        .get(format!("{foreign_base}/v1/runtime/status"))
        .header("X-Deploy-Token", foreign_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(final_status["data"]["desired"], "stopped");
    assert_eq!(
        final_status["data"]["runtime_instance_id"],
        initial_status["data"]["runtime_instance_id"]
    );
    assert_eq!(
        std::fs::read(foreign_root.join("identity.json")).unwrap(),
        identity_bytes
    );
    assert_eq!(
        std::fs::read(foreign_root.join("desired.json")).unwrap(),
        desired_bytes,
        "current Source must not change the foreign business or management boot intent"
    );
    assert!(
        std::fs::read(foreign_root.join("token")).unwrap() == token_bytes,
        "the foreign file credential bytes must remain unchanged"
    );
    assert_eq!(
        std::fs::metadata(foreign_root.join("owner.lock"))
            .unwrap()
            .ino(),
        lock_inode
    );
    assert!(
        runtime_supervisor::Owner::try_acquire(&foreign_root)
            .unwrap()
            .is_none()
    );
}
