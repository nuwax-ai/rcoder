//! Real child/group fixtures: no model, package download, shared port or global
//! environment mutation. Barriers are written by the actual signal handler.
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::supervise::{ChildExit, SupervisedChild};
use super::support::lock;
use super::types::{DevServerManager, LocalLaunch};
use crate::models::DevProcess;
use shared_types::ExecutorStopOutcome;

const SIGNAL_BARRIER: &str = r#"
import os, signal, sys, time
root = sys.argv[1]
def stop(signum, frame):
    open(os.path.join(root, 'term-entered'), 'w').write('signal received')
    while not os.path.exists(os.path.join(root, 'release')):
        time.sleep(.005)
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
open(os.path.join(root, 'ready'), 'w').write(str(os.getpid()))
while True:
    time.sleep(.01)
"#;

async fn barrier(path: &Path) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !path.try_exists().unwrap() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("actual fixture barrier must arrive");
}

fn manager(root: &Path) -> Arc<DevServerManager> {
    Arc::new(DevServerManager::new(Arc::new(crate::Config {
        log_base_dir: root.join("logs"),
        dev_stop_check_interval_ms: 10,
        dev_stop_max_attempts: 100,
        ..Default::default()
    })))
}

fn register(
    manager: &DevServerManager,
    key: &str,
    child: &Arc<SupervisedChild>,
    root: &Path,
    instance: Option<&str>,
) {
    let port = manager.port_pool.allocate(key).unwrap();
    let mut launches = lock(&manager.launches).unwrap();
    let mut processes = lock(&manager.processes).unwrap();
    let mut children = lock(&manager.supervised).unwrap();
    launches.insert(
        key.into(),
        LocalLaunch {
            launch_id: "original-owned-launch".into(),
            state_root: None,
        },
    );
    processes.insert(
        key.into(),
        DevProcess {
            pid: child.pid(),
            port,
            project_id: key.into(),
            started_at: 1,
            instance_id: instance.map(str::to_owned),
            base_path: None,
            log_dir: root.join("logs"),
            temp_log_name: String::new(),
            external_owner: None,
        },
    );
    children.insert(key.into(), child.clone());
}

fn spawn(script: &str, root: &Path) -> Arc<SupervisedChild> {
    let mut command = tokio::process::Command::new("python3");
    command
        .args(["-u", "-c", script])
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = process_utils::managed_tree::spawn_managed(command).unwrap();
    SupervisedChild::adopt_managed(child, Arc::new(Default::default())).unwrap()
}

async fn assert_reaped_and_retired(manager: &DevServerManager, key: &str, child: &SupervisedChild) {
    assert!(matches!(
        child.wait_exit(Duration::from_secs(2)).await,
        Some(ChildExit::Exited(_))
    ));
    assert!(child.tree_exit_confirmed());
    assert!(
        !process_utils::execution_tree::has_live_members(child.pid())
            .await
            .unwrap()
    );
    assert!(!lock(&manager.processes).unwrap().contains_key(key));
    assert!(!lock(&manager.supervised).unwrap().contains_key(key));
    assert!(!lock(&manager.launches).unwrap().contains_key(key));
    assert_eq!(manager.port_pool.status().unwrap().total_allocated, 0);
    assert!(!manager.has_uncleaned_cleanup(key).unwrap());
}

#[tokio::test]
async fn cancelled_legacy_stop_retains_owned_child_until_same_entry_retry_observes_exit() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager(root.path());
    let key = "legacy-cancelled-owned";
    let child = spawn(SIGNAL_BARRIER, root.path());
    register(&manager, key, &child, root.path(), None);
    barrier(&root.path().join("ready")).await;
    let stopped_manager = manager.clone();
    let stop = tokio::spawn(async move { stopped_manager.stop_dev(key).await });
    barrier(&root.path().join("term-entered")).await;
    stop.abort();
    assert!(stop.await.unwrap_err().is_cancelled());
    let retained = lock(&manager.supervised)
        .unwrap()
        .get(key)
        .cloned()
        .unwrap();
    assert!(Arc::ptr_eq(&retained, &child));
    assert!(manager.has_uncleaned_cleanup(key).unwrap());
    assert_eq!(manager.port_pool.status().unwrap().total_allocated, 1);
    std::fs::write(root.path().join("release"), b"exit now").unwrap();
    let stopped = manager.stop_dev(key).await.unwrap();
    assert!(
        stopped
            .killed_pids
            .iter()
            .any(|value| value.pid == child.pid() && value.killed)
    );
    assert_reaped_and_retired(&manager, key, &child).await;
    assert!(manager.stop_dev(key).await.unwrap().killed_pids.is_empty());
}

#[tokio::test]
async fn concurrent_coordinated_stops_observe_owned_tree_once_and_preserve_instance_gate() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager(root.path());
    let key = "coordinated-owned-stop";
    let instance = "original-instance";
    let child = spawn(SIGNAL_BARRIER, root.path());
    register(&manager, key, &child, root.path(), Some(instance));
    barrier(&root.path().join("ready")).await;
    assert_eq!(
        manager
            .stop_coordinated(key, "another-instance")
            .await
            .unwrap(),
        ExecutorStopOutcome::IdentityMismatch
    );
    assert!(!root.path().join("term-entered").exists());
    let first_manager = manager.clone();
    let first = tokio::spawn(async move { first_manager.stop_coordinated(key, instance).await });
    barrier(&root.path().join("term-entered")).await;
    let second_manager = manager.clone();
    let mut second =
        tokio::spawn(async move { second_manager.stop_coordinated(key, instance).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err()
    );
    assert!(lock(&manager.processes).unwrap().contains_key(key));
    assert_eq!(manager.port_pool.status().unwrap().total_allocated, 1);
    std::fs::write(root.path().join("release"), b"exit now").unwrap();
    assert_eq!(first.await.unwrap().unwrap(), ExecutorStopOutcome::Stopped);
    assert_eq!(
        second.await.unwrap().unwrap(),
        ExecutorStopOutcome::NotRegistered
    );
    assert_reaped_and_retired(&manager, key, &child).await;
}

const ORPHANED_HTTP: &str = r#"
import json, os, subprocess, sys, time
root = sys.argv[1]
child_script = '''
import http.server, json, os, sys
root = sys.argv[1]
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b'original descendant')
    def log_message(self, *args): pass
server = http.server.HTTPServer(('127.0.0.1', 0), Handler)
pending = os.path.join(root, 'child.json.pending')
with open(pending, 'w') as output:
    json.dump({'port':server.server_port, 'pid':os.getpid()}, output)
os.replace(pending, os.path.join(root, 'child.json'))
server.serve_forever()
'''
subprocess.Popen([sys.executable, '-u', '-c', child_script, root])
while not os.path.exists(os.path.join(root, 'release-root')): time.sleep(.005)
"#;

#[tokio::test]
async fn exited_root_is_not_tree_exit_until_live_http_descendant_is_drained() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager(root.path());
    let key = "legacy-owned-descendant";
    let child = spawn(ORPHANED_HTTP, root.path());
    register(&manager, key, &child, root.path(), None);
    barrier(&root.path().join("child.json")).await;
    let info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("child.json")).unwrap()).unwrap();
    let address = format!("http://127.0.0.1:{}/", info["port"].as_u64().unwrap());
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    assert_eq!(
        client
            .get(&address)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "original descendant"
    );
    std::fs::write(root.path().join("release-root"), b"root exits first").unwrap();
    assert!(
        matches!(child.wait_exit(Duration::from_secs(2)).await, Some(ChildExit::Exited(status)) if status.success())
    );
    assert!(!child.tree_exit_confirmed());
    assert_eq!(
        client.get(&address).send().await.unwrap().status(),
        reqwest::StatusCode::OK
    );
    let stopped = manager.stop_dev(key).await.unwrap();
    assert!(stopped.killed_pids.iter().any(|value| value.killed));
    assert!(client.get(&address).send().await.is_err());
    assert_reaped_and_retired(&manager, key, &child).await;
}

#[tokio::test]
async fn legacy_exited_candidates_do_not_wait_for_external_parent_reap() {
    legacy_candidate_case(false).await;
}

#[tokio::test]
async fn owned_legacy_tree_also_finishes_an_exited_additional_project_candidate() {
    legacy_candidate_case(true).await;
}

async fn legacy_candidate_case(managed: bool) {
    use std::os::unix::process::CommandExt;
    let root = tempfile::tempdir().unwrap();
    let key = format!("candidate-{}", uuid::Uuid::new_v4().simple());
    let path = root.path().join(&key);
    std::fs::create_dir(&path).unwrap();
    let manager = manager(&path);
    let mut first_command = tokio::process::Command::new("python3");
    first_command
        .args(["-c", "import time; time.sleep(60)"])
        .arg(&path)
        .process_group(0);
    let first = if managed {
        SupervisedChild::adopt_managed(
            process_utils::managed_tree::spawn_managed(first_command).unwrap(),
            Arc::new(Default::default()),
        )
        .unwrap()
    } else {
        SupervisedChild::adopt(first_command.spawn().unwrap(), Arc::new(Default::default()))
    };
    register(&manager, &key, &first, &path, None);
    // Its real parent retains this Child without wait/reap. Stop must establish
    // no live execution and complete even while the zombie PID slot remains.
    let mut second = std::process::Command::new("python3")
        .args(["-c", "import time; time.sleep(60)"])
        .arg(&path)
        .process_group(0)
        .spawn()
        .unwrap();
    let second_pid = second.id();
    let outcome = manager.stop_dev(&key).await;
    let retired = !lock(&manager.processes).unwrap().contains_key(&key);
    let port_released = manager.port_pool.status().unwrap().total_allocated == 0;
    let unreaped_slot = process_utils::process_exists(second_pid).unwrap();
    let second_has_live = process_utils::execution_tree::has_live_members(second_pid).await;
    // Original parent cleanup follows the observation, never enables Stop.
    if second_has_live.as_ref().is_ok_and(|live| *live) {
        second.kill().unwrap();
    }
    let second_exit = second.wait().unwrap();
    let original_exit = first.wait_exit(Duration::from_secs(2)).await;
    outcome.unwrap();
    assert!(retired && port_released);
    assert!(
        unreaped_slot,
        "fixture must retain the real zombie PID through Stop completion"
    );
    assert!(!second_has_live.unwrap());
    assert!(!second_exit.success());
    assert!(matches!(original_exit, Some(ChildExit::Exited(_))));
    assert!(!manager.has_uncleaned_cleanup(&key).unwrap());
}

const PREVIEW_HTTP: &str = r#"
import http.server, os, signal, sys, time
root, port = sys.argv[1], int(sys.argv[2])
def stop(signum, frame):
    open(os.path.join(root, 'term-entered'), 'w').write('real signal')
    while not os.path.exists(os.path.join(root, 'release')): time.sleep(.005)
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.end_headers()
        self.wfile.write(open(os.path.join(root, 'marker'), 'rb').read())
    def log_message(self, *args): pass
http.server.HTTPServer(('127.0.0.1', port), Handler).serve_forever()
"#;

/// Exercise the unchanged public Start/Stop methods and full preview launch
/// path. Package-manager preCmds are explicit no-download fixtures; the Python
/// HTTP child, OS signal, wait/reap, registration and port lifecycle are real.
/// A subprocess supplies PATH/override config without mutating global test env.
#[tokio::test]
async fn original_start_recovers_pending_owned_cleanup_and_serves_new_http() {
    const ENVIRONMENT: &str = "PHASE4_ISOLATED_PREVIEW_ROOT";
    if let Some(root) = std::env::var_os(ENVIRONMENT) {
        let root = std::path::PathBuf::from(root);
        let workspace = root.join("original-preview-entry");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let manager = Arc::new(DevServerManager::new(Arc::new(crate::Config {
            log_base_dir: root.join("logs"),
            dev_port_range_start: port,
            dev_port_range_end: port,
            dev_alive_max_wait_ms: 2_000,
            dev_alive_poll_interval_ms: 10,
            dev_alive_check_timeout_ms: 100,
            dev_stop_check_interval_ms: 10,
            dev_stop_max_attempts: 100,
            ..Default::default()
        })));
        let key = "original-preview-entry";
        let first = manager
            .start_dev(
                key,
                &workspace,
                super::DevLaunch {
                    base_path: None,
                    hooks: None,
                    pg: None,
                    request_context: None,
                    artifact_release_id: None,
                },
            )
            .await
            .unwrap();
        let original = manager.supervised_child(key).unwrap();
        assert!(original.has_managed_tree());
        use std::os::unix::process::CommandExt;
        let mut extra_command = std::process::Command::new("python3");
        extra_command
            .args(["-c", "import time; time.sleep(60)"])
            .arg(&workspace)
            .process_group(0);
        struct ExtraGuard(std::process::Child);
        impl Drop for ExtraGuard {
            fn drop(&mut self) {
                if self.0.try_wait().is_ok_and(|exit| exit.is_none()) {
                    let _kill = self.0.kill();
                }
                let _exit = self.0.wait();
            }
        }
        let mut extra = ExtraGuard(extra_command.spawn().unwrap());
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let address = format!("http://127.0.0.1:{}/proxy/{}/", first.port, first.port);
        assert_eq!(
            client
                .get(&address)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "A"
        );
        let stopped_manager = manager.clone();
        let stop = tokio::spawn(async move { stopped_manager.stop_dev(key).await });
        barrier(&root.join("term-entered")).await;
        stop.abort();
        assert!(stop.await.unwrap_err().is_cancelled());
        assert!(manager.has_uncleaned_cleanup(key).unwrap());
        std::fs::write(root.join("release"), b"original exits").unwrap();
        std::fs::write(root.join("marker"), b"B").unwrap();
        let second = manager
            .start_dev(
                key,
                &workspace,
                super::DevLaunch {
                    base_path: None,
                    hooks: None,
                    pg: None,
                    request_context: None,
                    artifact_release_id: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            original.wait_exit(Duration::from_secs(2)).await,
            Some(ChildExit::Exited(_))
        ));
        assert!(
            process_utils::process_exists(extra.0.id()).unwrap(),
            "raw legacy parent must still own the zombie PID while new HTTP serves"
        );
        assert!(
            !process_utils::execution_tree::has_live_members(extra.0.id())
                .await
                .unwrap()
        );
        let _parent_reap = extra.0.wait().unwrap();
        assert_eq!(second.port, first.port);
        let replacement = manager.supervised_child(key).unwrap();
        assert!(!Arc::ptr_eq(&original, &replacement));
        assert_eq!(
            client
                .get(&address)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "B"
        );
        super::process::kill_process_group_force(replacement.pid());
        assert!(matches!(
            replacement.wait_exit(Duration::from_secs(2)).await,
            Some(ChildExit::Exited(_))
        ));
        assert!(!manager.has_uncleaned_cleanup(key).unwrap());
        std::fs::write(root.join("marker"), b"C").unwrap();
        let recovered = manager
            .start_dev(
                key,
                &workspace,
                super::DevLaunch {
                    base_path: None,
                    hooks: None,
                    pg: None,
                    request_context: None,
                    artifact_release_id: None,
                },
            )
            .await
            .unwrap();
        let replacement = manager.supervised_child(key).unwrap();
        assert_eq!(
            client
                .get(&address)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "C"
        );
        let repeated = manager
            .start_dev(
                key,
                &workspace,
                super::DevLaunch {
                    base_path: None,
                    hooks: None,
                    pg: None,
                    request_context: None,
                    artifact_release_id: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            (repeated.pid, repeated.port),
            (recovered.pid, recovered.port)
        );
        assert_eq!(lock(&manager.supervised).unwrap().len(), 1);
        assert!(!manager.has_uncleaned_cleanup(key).unwrap());
        manager.stop_dev(key).await.unwrap();
        assert_reaped_and_retired(&manager, key, &replacement).await;
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("original-preview-entry")).unwrap();
    std::fs::create_dir(root.path().join("bin")).unwrap();
    std::fs::write(root.path().join("marker"), b"A").unwrap();
    let server = root.path().join("preview.py");
    std::fs::write(&server, PREVIEW_HTTP).unwrap();
    let pnpm = root.path().join("bin/pnpm");
    std::fs::write(&pnpm, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(pnpm, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut paths = vec![root.path().join("bin")];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let override_command = format!(
        "exec python3 -u {} {} {{PORT}}",
        shlex::try_quote(server.to_str().unwrap()).unwrap(),
        shlex::try_quote(root.path().to_str().unwrap()).unwrap()
    );
    let output = tokio::time::timeout(Duration::from_secs(30), tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "service::dev_server::owned_stop_tests::original_start_recovers_pending_owned_cleanup_and_serves_new_http", "--nocapture"])
        .env(ENVIRONMENT, root.path()).env("DEV_SERVER_OVERRIDE_CMD", override_command)
        .env("PATH", std::env::join_paths(paths).unwrap()).kill_on_drop(true).output()).await.unwrap().unwrap();
    assert!(
        output.status.success(),
        "isolated public-entry fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn legacy_stop_cleanup_preserves_successor_temp_log_after_retirement_barrier() {
    legacy_temp_log_cleanup_case(false, None).await;
}

#[tokio::test]
async fn legacy_stop_without_captured_log_name_preserves_all_temporary_logs() {
    legacy_temp_log_cleanup_case(true, None).await;
}

#[tokio::test]
async fn legacy_stop_cleanup_preserves_same_millisecond_successor_log() {
    legacy_temp_log_cleanup_case(false, Some([100, 100, 100])).await;
}

#[tokio::test]
async fn legacy_stop_cleanup_preserves_clock_rollback_successor_log() {
    // The original name sampled 100; by Stop the clock was 200, and the next
    // launch samples 100 again. No host clock is changed by this fixture.
    legacy_temp_log_cleanup_case(false, Some([100, 200, 100])).await;
}

async fn legacy_temp_log_cleanup_case(unknown_name: bool, clock: Option<[i64; 3]>) {
    let root = tempfile::tempdir().unwrap();
    let manager = manager(root.path());
    let key = "legacy-log-successor";
    let child = spawn(SIGNAL_BARRIER, root.path());
    register(&manager, key, &child, root.path(), None);
    barrier(&root.path().join("ready")).await;
    let logs = root.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let original_launch = "0199861c786770008000000000000001";
    let successor_launch = "0199861c786770008000000000000002";
    let (old_name, next_name) = match clock {
        Some([original, observed_at_stop, next]) => {
            assert!(observed_at_stop >= original && observed_at_stop >= next);
            (
                super::log::launch_temp_log_name(original, original_launch),
                super::log::launch_temp_log_name(next, successor_launch),
            )
        }
        None => ("dev-temp-100.log".into(), "dev-temp-200.log".into()),
    };
    std::fs::write(logs.join(&old_name), b"original log").unwrap();
    lock(&manager.launches)
        .unwrap()
        .get_mut(key)
        .unwrap()
        .launch_id = original_launch.into();
    std::fs::write(logs.join("dev-temp-50.log"), b"unrelated retained log").unwrap();
    std::fs::write(logs.join("dev-current.log"), b"main log").unwrap();
    lock(&manager.processes)
        .unwrap()
        .get_mut(key)
        .unwrap()
        .temp_log_name = if unknown_name {
        String::new()
    } else {
        old_name.clone()
    };
    std::fs::write(root.path().join("release"), b"original child exits on Stop").unwrap();
    let (retired, retirement) = tokio::sync::oneshot::channel();
    let release_cleanup = Arc::new(tokio::sync::Notify::new());
    let released = release_cleanup.clone();
    let stopping = manager.clone();
    // The entire real legacy observation/retirement path runs unchanged. Only
    // its final log I/O waits at this deterministic scheduling barrier.
    let stop = tokio::spawn(async move {
        stopping
            .stop_legacy_observed_with_log_cleanup(key, move |captured| async move {
                retired.send(()).unwrap();
                released.notified().await;
                super::log::cleanup_temp_log(&captured.log_dir, &captured.temp_log_name).await;
            })
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), retirement)
        .await
        .expect("real Stop must retire its original registration before log cleanup")
        .unwrap();
    assert!(matches!(
        child.wait_exit(Duration::from_secs(2)).await,
        Some(ChildExit::Exited(_))
    ));
    assert!(!lock(&manager.processes).unwrap().contains_key(key));
    assert_eq!(manager.port_pool.status().unwrap().total_allocated, 0);
    // A real successor Child and registration are a controlled publication
    // fixture here; this test does not claim a second full package-manager run.
    let successor_root = root.path().join("successor");
    std::fs::create_dir(&successor_root).unwrap();
    let successor = spawn(SIGNAL_BARRIER, &successor_root);
    barrier(&successor_root.join("ready")).await;
    register(&manager, key, &successor, &successor_root, None);
    {
        lock(&manager.launches)
            .unwrap()
            .get_mut(key)
            .unwrap()
            .launch_id = successor_launch.into();
        let mut processes = lock(&manager.processes).unwrap();
        let process = processes.get_mut(key).unwrap();
        process.started_at = 2;
        process.log_dir = logs.clone();
        process.temp_log_name = next_name.clone();
    }
    std::fs::write(logs.join(&next_name), b"successor startup log").unwrap();
    release_cleanup.notify_one();
    let stopped = stop.await.unwrap().unwrap();
    assert!(
        stopped
            .killed_pids
            .iter()
            .any(|value| value.pid == child.pid() && value.killed)
    );
    // Capture I/O now but assert after reaping the real successor, so even the
    // pre-fix RED leaves neither a Child nor an allocated port behind.
    let successor_log = std::fs::read(logs.join(&next_name));
    assert_eq!(
        std::fs::read(logs.join("dev-temp-50.log")).unwrap(),
        b"unrelated retained log"
    );
    assert_eq!(
        std::fs::read(logs.join("dev-current.log")).unwrap(),
        b"main log"
    );
    let original_log_exists = logs.join(&old_name).exists();
    assert!(successor.exited().is_none());
    assert!(!successor_root.join("term-entered").exists());
    assert!(Arc::ptr_eq(
        &successor,
        &manager.supervised_child(key).unwrap()
    ));
    assert_eq!(manager.port_pool.status().unwrap().total_allocated, 1);
    std::fs::write(successor_root.join("release"), b"successor cleanup").unwrap();
    manager.stop_dev(key).await.unwrap();
    assert_reaped_and_retired(&manager, key, &successor).await;
    assert_eq!(successor_log.unwrap(), b"successor startup log");
    assert_eq!(original_log_exists, unknown_name);
}
