//! Windows owner crash must close the job and stop descendants without taskkill /T.
#![cfg(windows)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(100),
    )
    .is_ok()
}

#[test]
fn windows_owner_crash_closes_managed_job_and_grandchild() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("code");
    std::fs::create_dir_all(workspace.join("web")).unwrap();
    let held: Vec<_> = (0..3)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let ports: Vec<_> = held
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .collect();
    let fixture = toml::Value::String(env!("CARGO_BIN_EXE_tree-fixture").into()).to_string();
    // A real service starts a grandchild. Missing Pingap enters the ordinary
    // shutdown grace period, during which we kill the owner itself. No PG is used.
    std::fs::write(
        workspace.join("release.lock.toml"),
        format!(
            r#"
schema_version = 1
release_id = "windows-owner-crash"
workspace_name = "windows-owner-crash"
minimum_app_cli_version = "0.3.6"
runtime_image_digest = "local-test"
[pingap]
mode = "managed"
version = "0.14.3"
commit = "local-test"
[[services]]
service_id = "web"
name = "Web"
dir = "web"
type = "node"
kind = "web"
enabled = true
logs = []
env = {{}}
port = {}
[services.run]
command = [{fixture}, "serve", "{}", "{}"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 60
[services.health]
startup_path = "/health"
readiness_path = "/health"
liveness_path = "/health"
[services.proxy]
path = "/web"
strip_prefix = true
"#,
            ports[0], ports[0], ports[1]
        ),
    )
    .unwrap();
    drop(held);
    let log_path = root.path().join("owner.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut owner = Command::new(env!("CARGO_BIN_EXE_app-cli"))
        .arg("run")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--log-dir")
        .arg(root.path().join("logs"))
        .arg("--admin-addr")
        .arg(format!("127.0.0.1:{}", ports[2]))
        .env("APP_CLI_STATE_ROOT", root.path().join("state"))
        .env("PROJECT_ID", "ownercrashtest")
        .env_remove("APP_DEPLOY_URL")
        .env_remove("APP_RELEASE_ID")
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let ready = loop {
        if listening(ports[0]) && listening(ports[1]) {
            break true;
        }
        if owner.try_wait().unwrap().is_some() || Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    // Capture only this fixture's listeners for cleanup if the pre-fix test fails.
    let query = format!(
        "Get-NetTCPConnection -State Listen | Where-Object {{$_.LocalPort -in @({},{})}} | ForEach-Object {{$_.OwningProcess}}",
        ports[0], ports[1]
    );
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &query])
        .output()
        .unwrap();
    let pids: Vec<u32> = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|v| v.parse().ok())
        .collect();
    let _ = owner.kill(); // TerminateProcess(owner), deliberately not tree kill.
    let _ = owner.wait();
    let deadline = Instant::now() + Duration::from_secs(5);
    while (listening(ports[0]) || listening(ports[1])) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let closed = !listening(ports[0]) && !listening(ports[1]);
    if !closed {
        for pid in pids {
            let _ = Command::new("taskkill.exe")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output();
        }
    }
    assert!(
        ready,
        "real service root and grandchild must both listen before killing owner: {}",
        std::fs::read_to_string(log_path).unwrap()
    );
    assert!(
        closed,
        "owner crash must close its job and release root/grandchild ports"
    );
}
