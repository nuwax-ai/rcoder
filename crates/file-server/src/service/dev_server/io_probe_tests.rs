//! Filesystem observation failures must not choose another execution engine or
//! erase an existing project's local-supervision observation.
use super::super::{DevLaunch, DevServerManager, discovery};
use crate::error::AppError;
use crate::models::DevProcess;
use shared_types::{ExecutorStartTicket, ExecutorStopOutcome};
use std::os::unix::{fs::symlink, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;

struct Fixture {
    _directory: tempfile::TempDir,
    workspace: PathBuf,
    logs: PathBuf,
    manager: DevServerManager,
    project: String,
}

impl Fixture {
    fn new(block_legacy_logs: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let project = format!("io-probe-{}", uuid::Uuid::new_v4().simple());
        let workspace = directory.path().join(&project);
        std::fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let logs = directory.path().join("logs");
        if block_legacy_logs {
            // The unfixed implementation reaches legacy log preparation. Stop
            // it there, before any pnpm/preCmd/network or user command can run.
            std::fs::write(&logs, b"block unintended legacy pipeline").unwrap();
        }
        let config = crate::Config {
            log_base_dir: logs.clone(),
            app_cli_bin: Some("/bin/false".into()),
            app_cli_admin_probe_addr: "127.0.0.1:1".into(),
            dev_stop_check_interval_ms: 5,
            dev_stop_max_attempts: 2,
            ..crate::Config::default()
        };
        Self {
            _directory: directory,
            workspace,
            logs,
            manager: DevServerManager::new(Arc::new(config)),
            project,
        }
    }

    fn loop_file(path: &Path) -> std::io::Error {
        symlink(path.file_name().unwrap(), path).unwrap();
        path.try_exists()
            .expect_err("self-referential symlink must produce a real probe IO error")
    }

    fn manifest_loop(&self) -> std::io::Error {
        Self::loop_file(&self.workspace.join("workspace.manifest.toml"))
    }

    fn ticket(&self) -> ExecutorStartTicket {
        ExecutorStartTicket {
            preview_key: self.project.clone(),
            log_key: self.project.clone(),
            project_path: self.workspace.display().to_string(),
            port: 19001,
            base_path: None,
            instance_id: "captured-preview-instance".into(),
        }
    }

    fn register_legacy_child(&self, child: &OwnedChild, coordinated: bool) -> u16 {
        let port = if coordinated {
            self.ticket().port
        } else {
            self.manager.port_pool.allocate(&self.project).unwrap()
        };
        self.manager.processes.lock().unwrap().insert(
            self.project.clone(),
            DevProcess {
                pid: child.0.id(),
                port,
                project_id: self.project.clone(),
                started_at: 1,
                instance_id: coordinated.then(|| self.ticket().instance_id),
                base_path: None,
                log_dir: self.logs.clone(),
                temp_log_name: String::new(),
                external_owner: None,
            },
        );
        port
    }

    fn assert_idle_maps(&self) {
        assert!(self.manager.starting.lock().unwrap().is_empty());
        assert!(self.manager.processes.lock().unwrap().is_empty());
        assert!(self.manager.launches.lock().unwrap().is_empty());
        assert!(self.manager.supervised.lock().unwrap().is_empty());
        assert_eq!(self.manager.port_pool.status().unwrap().total_allocated, 0);
    }

    fn assert_no_legacy_preparation(&self) {
        assert!(!self.workspace.join(".npmrc").exists());
        assert!(!self.logs.is_dir(), "legacy log/preCmd preparation ran");
        self.assert_idle_maps();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // The OwnedChild guard controls the only real child created here. Do
        // not let manager Drop signal a numeric PID after the guard reaped it.
        self.manager.processes.lock().unwrap().clear();
    }
}

struct OwnedChild(Child);

impl OwnedChild {
    fn spawn() -> Self {
        Self(
            Command::new("sleep")
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap(),
        )
    }

    fn wait(&mut self) -> ExitStatus {
        self.0.wait().unwrap()
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            let _kill_result = self.0.kill();
        }
        let _wait_result = self.0.wait();
    }
}

fn launch() -> DevLaunch<'static> {
    DevLaunch {
        base_path: None,
        hooks: None,
        pg: None,
        request_context: None,
        artifact_release_id: None,
    }
}

fn assert_original_io(error: AppError, original: std::io::Error) {
    let expected = AppError::from(original);
    assert_eq!(
        std::mem::discriminant(&error),
        std::mem::discriminant(&expected)
    );
    assert_eq!(error.to_string(), expected.to_string());
}

#[tokio::test]
async fn manifest_probe_io_direct_start_never_enters_legacy_preparation() {
    let fixture = Fixture::new(true);
    let original = fixture.manifest_loop();
    let error = fixture
        .manager
        .start_dev(&fixture.project, &fixture.workspace, launch())
        .await
        .expect_err("manifest probe IO must not enter legacy pipeline");
    assert_original_io(error, original);
    fixture.assert_no_legacy_preparation();
}

#[tokio::test]
async fn manifest_probe_io_coordinated_start_never_enters_legacy_preparation() {
    let fixture = Fixture::new(true);
    let original = fixture.manifest_loop();
    let error = fixture
        .manager
        .start_coordinated(&fixture.ticket())
        .await
        .expect_err("manifest probe IO must not enter preview pipeline");
    assert_original_io(error, original);
    fixture.assert_no_legacy_preparation();
}

#[tokio::test]
async fn manifest_probe_io_direct_start_preserves_registration_and_retries_after_repair() {
    let fixture = Fixture::new(false);
    let mut child = OwnedChild::spawn();
    let port = fixture.register_legacy_child(&child, false);
    let original = fixture.manifest_loop();
    let error = fixture
        .manager
        .start_dev(&fixture.project, &fixture.workspace, launch())
        .await
        .expect_err("IO is not permission to reuse an unclassified legacy process");
    assert_original_io(error, original);
    assert!(fixture.manager.starting.lock().unwrap().is_empty());
    assert_eq!(
        fixture.manager.processes.lock().unwrap()[&fixture.project].pid,
        child.0.id()
    );
    assert_eq!(
        fixture.manager.port_pool.status().unwrap().total_allocated,
        1
    );
    assert!(!fixture.logs.exists());
    assert!(!fixture.workspace.join(".npmrc").exists());

    std::fs::remove_file(fixture.workspace.join("workspace.manifest.toml")).unwrap();
    let started = fixture
        .manager
        .start_dev(&fixture.project, &fixture.workspace, launch())
        .await
        .unwrap();
    assert_eq!((started.pid, started.port), (child.0.id(), port));
    let stopped = fixture.manager.stop_dev(&fixture.project).await.unwrap();
    assert!(
        stopped
            .killed_pids
            .iter()
            .any(|row| row.pid == child.0.id() && row.killed)
    );
    let _actual_exit = child.wait();
    fixture.assert_idle_maps();
}

#[tokio::test]
async fn manifest_probe_io_coordinated_start_preserves_registration_and_retries_after_repair() {
    let fixture = Fixture::new(false);
    let mut child = OwnedChild::spawn();
    let port = fixture.register_legacy_child(&child, true);
    let original = fixture.manifest_loop();
    let error = fixture
        .manager
        .start_coordinated(&fixture.ticket())
        .await
        .expect_err("IO is not permission to reuse an unclassified preview process");
    assert_original_io(error, original);
    assert!(fixture.manager.starting.lock().unwrap().is_empty());
    assert_eq!(
        fixture.manager.processes.lock().unwrap()[&fixture.project].pid,
        child.0.id()
    );
    assert!(!fixture.logs.exists());
    assert!(!fixture.workspace.join(".npmrc").exists());

    std::fs::remove_file(fixture.workspace.join("workspace.manifest.toml")).unwrap();
    let started = fixture
        .manager
        .start_coordinated(&fixture.ticket())
        .await
        .unwrap();
    assert_eq!((started.pid, started.port), (child.0.id(), port));
    assert_eq!(
        fixture
            .manager
            .stop_coordinated(&fixture.project, &fixture.ticket().instance_id)
            .await
            .unwrap(),
        ExecutorStopOutcome::Stopped
    );
    let _actual_exit = child.wait();
    fixture.assert_idle_maps();
}

#[tokio::test]
async fn manifest_probe_present_keeps_userapp_out_of_coordinated_preview() {
    let fixture = Fixture::new(true);
    std::fs::write(
        fixture.workspace.join("workspace.manifest.toml"),
        "# marker\n",
    )
    .unwrap();
    let error = fixture
        .manager
        .start_coordinated(&fixture.ticket())
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Business(ref message)
        if message == "workspace manifest projects are userapp domain, not coordinated previews"));
    fixture.assert_no_legacy_preparation();
}

fn supervision_record(root: &Path, workspace: &Path, component: &str) -> Vec<u8> {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1, "instance": "fixture-supervisor", "address": "127.0.0.1:1", "token": "fixture",
        "requests": [], "snapshot": {
            "version": 1, "binding": {"component": component, "resource": workspace},
            "supervisor_id": "fixture-supervisor", "generation": null, "phase": "stopped",
            "intent": "run", "operation_id": null, "error": null, "problem": null
        }
    })).unwrap();
    std::fs::write(root.join("supervisor.json"), &bytes).unwrap();
    bytes
}

#[test]
fn supervisor_probe_io_is_observation_failure_and_repair_preserves_original_entry() {
    let fixture = Fixture::new(false);
    let root = runtime_state_layout::ensure_state_root(&fixture.workspace, None, None).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let original = Fixture::loop_file(&root.join("supervisor.json"));
    let report = discovery::discover_targets_with(&fixture.workspace, std::slice::from_ref(&root));
    assert!(report.targets.is_empty());
    assert_eq!(
        report.unreadable.len(),
        1,
        "probe IO was mistaken for an absent owner"
    );
    assert_eq!(report.unreadable[0].0, root);
    assert!(report.unreadable[0].1.contains(&original.to_string()));
    assert!(report.has_local_presence());
    assert!(
        std::fs::symlink_metadata(root.join("supervisor.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    std::fs::remove_file(root.join("supervisor.json")).unwrap();
    let original = supervision_record(&root, &fixture.workspace, "app-cli");
    let repaired =
        discovery::discover_targets_with(&fixture.workspace, std::slice::from_ref(&root));
    assert!(repaired.unreadable.is_empty());
    assert_eq!(repaired.targets.len(), 1);
    assert_eq!(repaired.targets[0].binding().resource, fixture.workspace);
    assert_eq!(
        repaired.targets[0].snapshot.phase,
        runtime_supervisor::Phase::Stopped
    );
    assert_eq!(
        std::fs::read(root.join("supervisor.json")).unwrap(),
        original
    );
}

#[test]
fn supervisor_probe_missing_or_readable_foreign_records_do_not_block_this_project() {
    let fixture = Fixture::new(false);
    let root = runtime_state_layout::ensure_state_root(&fixture.workspace, None, None).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let missing = discovery::discover_targets_with(&fixture.workspace, std::slice::from_ref(&root));
    assert!(!missing.has_local_presence());
    for (workspace, component) in [
        (fixture.workspace.join("another-project"), "app-cli"),
        (fixture.workspace.clone(), "another-component"),
    ] {
        let original = supervision_record(&root, &workspace, component);
        let report =
            discovery::discover_targets_with(&fixture.workspace, std::slice::from_ref(&root));
        assert!(
            !report.has_local_presence(),
            "readable foreign records became a global gate"
        );
        assert_eq!(
            std::fs::read(root.join("supervisor.json")).unwrap(),
            original
        );
    }
}
