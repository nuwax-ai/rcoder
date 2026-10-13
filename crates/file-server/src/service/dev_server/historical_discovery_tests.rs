use super::DevServerManager;
use crate::Config;
use std::{path::PathBuf, sync::Arc};

struct Fixture {
    _temporary: tempfile::TempDir,
    workspace: PathBuf,
    root: PathBuf,
    manager: DevServerManager,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let workspace = temporary.path().join("project");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let root = runtime_state_layout::ensure_state_root(&workspace, None, None).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let mut config = Config::from_env().unwrap();
        config.log_base_dir = temporary.path().join("logs");
        config.app_cli_bin = Some("/bin/false".into());
        config.dev_supervision_stop_budget_secs = 2;
        Self {
            _temporary: temporary,
            workspace,
            root,
            manager: DevServerManager::new(Arc::new(config)),
        }
    }

    fn record(&self, foreign: bool) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version":999,"instance":"historical-owner","address":"127.0.0.1:1",
            "token":"fixture","requests":[],"snapshot":{
                "version":1,"binding":{"component":"app-cli","resource":
                    if foreign { self.workspace.join("other") } else { self.workspace.clone() }},
                "supervisor_id":"historical-owner","generation":null,"phase":"ready",
                "intent":"run","operation_id":null,"error":null,"problem":null
            }
        }))
        .unwrap()
    }

    fn save(&self, bytes: &[u8]) {
        std::fs::write(self.root.join("supervisor.json"), bytes).unwrap();
    }
}

#[tokio::test]
async fn historical_discovery_metadata_cannot_permanently_block_start_or_stop() {
    for malformed in [false, true] {
        let fixture = Fixture::new();
        let original = if malformed {
            b"{old damaged locator".to_vec()
        } else {
            fixture.record(false)
        };
        fixture.save(&original);
        fixture
            .manager
            .ensure_no_local_execution("project", &fixture.workspace)
            .await
            .unwrap();
        let snapshot = runtime_supervisor::last_snapshot(&fixture.root).unwrap();
        assert_eq!(snapshot.phase, runtime_supervisor::Phase::Stopped);
        assert_eq!(snapshot.binding.resource, fixture.workspace);
        // The current cleanup executable is /bin/false: an empty execution
        // tree needs no callback, and metadata repair cannot invent a worker.
        assert!(!fixture.root.join("work").exists());
        assert!(
            std::fs::read_dir(&fixture.root)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.path().is_file()
                    && std::fs::read(entry.path()).ok().as_deref() == Some(original.as_slice()))
        );
        fixture
            .manager
            .ensure_no_local_execution("project", &fixture.workspace)
            .await
            .unwrap();
        fixture
            .manager
            .stop_local_targets("project", &fixture.workspace)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn historical_discovery_recovery_keeps_real_owner_lock_and_foreign_workspace_protected() {
    for foreign in [false, true] {
        let fixture = Fixture::new();
        let original = fixture.record(foreign);
        fixture.save(&original);
        let owner = (!foreign).then(|| {
            runtime_supervisor::Owner::try_acquire(&fixture.root)
                .unwrap()
                .unwrap()
        });
        assert!(
            fixture
                .manager
                .ensure_no_local_execution("project", &fixture.workspace)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(fixture.root.join("supervisor.json")).unwrap(),
            original
        );
        assert!(!fixture.root.join("work").exists());
        drop(owner);
    }
}

impl Fixture {
    fn retained_stop(
        &self,
        malformed: bool,
    ) -> (String, runtime_supervisor::StopWorkAttempt, Vec<u8>) {
        let binding = runtime_supervisor::Binding {
            component: "app-cli".into(),
            resource: self.workspace.clone(),
        };
        let supervisor = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let work = self.root.join("work").join(&generation);
        std::fs::create_dir_all(&work).unwrap();
        // A real command's exit and quiescent receipt precede the fixture ledger.
        let command =
            process_utils::command_context::CommandRecord::prepare_in(work.join("commands"))
                .unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 17"])
            .spawn()
            .unwrap();
        command.running(Some(child.id())).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(17));
        command.quiescent().unwrap();
        std::fs::write(
            work.join("generation.json"),
            serde_json::to_vec(&serde_json::json!({
                "version":1,"id":generation,"supervisor":supervisor,"token":"retained-test-token",
                "intent":"stopped","phase":"Quiescent","worker_pid":null,"exit_code":17,
                "error":null,"physical_domain":null,"process_epoch":null
            }))
            .unwrap(),
        )
        .unwrap();
        let locator = serde_json::json!({
            "version":999,"instance":supervisor,"address":"127.0.0.1:1","token":"fixture","requests":[],
            "snapshot":{"version":1,"binding":binding,"supervisor_id":supervisor,"generation":generation,
                "phase":"ready","intent":"run","operation_id":null,"error":null,"problem":null}
        });
        let original = if malformed {
            b"{damaged captured locator".to_vec()
        } else {
            serde_json::to_vec(&locator).unwrap()
        };
        self.save(&original);
        let mut attempt = runtime_supervisor::StopWorkAttempt::allocate(&self.root, &binding);
        attempt.mode = runtime_supervisor::StopMode::OfflineChosen {
            supervisor_id: supervisor,
        };
        attempt.captured_generation = Some(generation.clone());
        attempt.request.expected_generation = Some(generation);
        let key = format!("project|{}", self.root.display());
        self.manager
            .external_transaction(|state| {
                state.local_stops.insert(
                    key.clone(),
                    super::types::LocalStopRecord::from_attempt(&attempt),
                );
                Ok(())
            })
            .unwrap();
        (key, attempt, original)
    }
}

#[tokio::test]
async fn historical_pending_stop_start_repairs_then_finishes_same_original_request() {
    for malformed in [false, true] {
        let fixture = Fixture::new();
        let (_, original_attempt, original_bytes) = fixture.retained_stop(malformed);
        // Recreate the manager to prove the disk request, not a memory cache,
        // survives a file-server restart and remains the completion identity.
        let manager = DevServerManager::new(Arc::clone(&fixture.manager.config));
        manager
            .ensure_no_local_execution("project", &fixture.workspace)
            .await
            .unwrap();
        let snapshot = runtime_supervisor::last_snapshot(&fixture.root).unwrap();
        assert_eq!(snapshot.phase, runtime_supervisor::Phase::Stopped);
        assert_eq!(
            snapshot.supervisor_id,
            match &original_attempt.mode {
                runtime_supervisor::StopMode::OfflineChosen { supervisor_id } =>
                    supervisor_id.clone(),
                _ => unreachable!(),
            }
        );
        assert_eq!(snapshot.generation, original_attempt.captured_generation);
        assert_eq!(
            snapshot.operation_id.as_deref(),
            Some(original_attempt.request.request_id.as_str())
        );
        assert!(manager.pending_local_stops("project").unwrap().is_empty());
        assert!(std::fs::read_dir(&fixture.root).unwrap().any(|entry| {
            std::fs::read(entry.unwrap().path()).ok().as_deref() == Some(original_bytes.as_slice())
        }));
        manager
            .ensure_no_local_execution("project", &fixture.workspace)
            .await
            .unwrap();
        assert_eq!(
            runtime_supervisor::last_snapshot(&fixture.root)
                .unwrap()
                .operation_id,
            snapshot.operation_id
        );
    }
}

#[tokio::test]
async fn historical_pending_stop_never_repairs_or_drains_a_changed_generation() {
    let fixture = Fixture::new();
    let (_, original_attempt, original_bytes) = fixture.retained_stop(false);
    let mut changed: serde_json::Value = serde_json::from_slice(&original_bytes).unwrap();
    changed["snapshot"]["generation"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
    let current_bytes = serde_json::to_vec(&changed).unwrap();
    fixture.save(&current_bytes);
    let ledger = fixture
        .root
        .join("work")
        .join(original_attempt.captured_generation.as_ref().unwrap())
        .join("generation.json");
    let ledger_before = std::fs::read(&ledger).unwrap();
    assert!(
        fixture
            .manager
            .ensure_no_local_execution("project", &fixture.workspace)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(fixture.root.join("supervisor.json")).unwrap(),
        current_bytes
    );
    assert_eq!(std::fs::read(&ledger).unwrap(), ledger_before);
    assert_eq!(
        fixture.manager.pending_local_stops("project").unwrap()[0].1,
        original_attempt
    );
}

#[tokio::test]
async fn historical_pending_unresolved_stop_is_captured_before_original_dispatch() {
    for malformed in [false, true] {
        let fixture = Fixture::new();
        let (key, captured, original_bytes) = fixture.retained_stop(malformed);
        let mut allocated =
            runtime_supervisor::StopWorkAttempt::allocate(&fixture.root, &captured.binding);
        // Allocation survives a manager crash, before target capture or I/O.
        allocated.request.request_id = captured.request.request_id.clone();
        fixture
            .manager
            .external_transaction(|state| {
                state.local_stops.insert(
                    key.clone(),
                    super::types::LocalStopRecord::from_attempt(&allocated),
                );
                Ok(())
            })
            .unwrap();
        let ledger = fixture
            .root
            .join("work")
            .join(captured.captured_generation.as_ref().unwrap())
            .join("generation.json");
        let original_ledger = std::fs::read(&ledger).unwrap();
        let manager = DevServerManager::new(Arc::clone(&fixture.manager.config));
        manager
            .ensure_no_local_execution("project", &fixture.workspace)
            .await
            .unwrap();
        let result = runtime_supervisor::last_snapshot(&fixture.root).unwrap();
        assert_eq!(result.phase, runtime_supervisor::Phase::Stopped);
        assert_eq!(
            result.operation_id.as_deref(),
            Some(allocated.request.request_id.as_str())
        );
        assert!(manager.pending_local_stops("project").unwrap().is_empty());
        assert_eq!(std::fs::read(&ledger).unwrap(), original_ledger);
        assert!(std::fs::read_dir(&fixture.root).unwrap().any(|entry| {
            std::fs::read(entry.unwrap().path()).ok().as_deref() == Some(original_bytes.as_slice())
        }));
    }
}

#[tokio::test]
async fn historical_pending_unresolved_stop_with_captured_parameters_is_preserved() {
    let fixture = Fixture::new();
    let (key, mut attempt, original_bytes) = fixture.retained_stop(true);
    attempt.mode = runtime_supervisor::StopMode::Unresolved;
    fixture
        .manager
        .external_transaction(|state| {
            state.local_stops.insert(
                key.clone(),
                super::types::LocalStopRecord::from_attempt(&attempt),
            );
            Ok(())
        })
        .unwrap();
    assert!(
        fixture
            .manager
            .ensure_no_local_execution("project", &fixture.workspace)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read(fixture.root.join("supervisor.json")).unwrap(),
        original_bytes
    );
    assert_eq!(
        fixture.manager.pending_local_stops("project").unwrap()[0].1,
        attempt
    );
}
