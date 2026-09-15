//! P1-01/P1-02 bin 级回归：API 预绑定 fail-fast、早期失败非零退出、
//! pingap 失败路径的 Done 终局事件 + 非零退出码。
//!
//! 走真实二进制（`CARGO_BIN_EXE_app-cli`）+ 受控 workspace / 端口 / pingap
//! 注入，不依赖 PG（`APP_CLI_SKIP_PG_WAIT`）与真实 pingap（`--pingap-bin
//! /bin/false`）。对应 tasks.md A01/A02/A04 的 bin 层覆盖（组件级）。
#![cfg(unix)]

use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// 最小合法 release.lock（node web 服务：spawn 失败走容错路径，不阻塞到达
/// start_pingap；pingap 注入 /bin/false 使编译阶段确定失败）。
const MINIMAL_LOCK: &str = r#"
schema_version = 1
release_id = "p1-bin-test-0001"
workspace_name = "p1-bin"
minimum_app_cli_version = "0.1.3"
runtime_image_digest = "registry.example/app-runtime:0.1.140"

[pingap]
mode = "managed"
version = "0.14.1"
commit = "abc123"

[[services]]
service_id = "web"
name = "Web"
dir = "web"
type = "node"
kind = "web"
enabled = true
port = 4200

[services.run]
command = ["node", "server.js"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 5

[services.health]
startup_path = "/health"
readiness_path = "/ready"
liveness_path = "/health"

[[services.logs]]
id = "application"
glob = "web*.log*"
format = "jsonl"

[services.env]
"#;

fn reserve_port() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let address = listener.local_addr().expect("local addr").to_string();
    (listener, address)
}

fn free_port_address() -> String {
    let (listener, address) = reserve_port();
    drop(listener);
    address
}

fn base_command(workspace: &Path, logs: &Path, admin_addr: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_app-cli"));
    command
        .args([
            "--workspace",
            workspace.to_str().expect("workspace path"),
            "--log-dir",
            logs.to_str().expect("log path"),
            "--admin-addr",
            admin_addr,
            "--pingap-bin",
            "/bin/false",
        ])
        .env_remove("APP_DEPLOY_URL")
        .env_remove("APP_RELEASE_ID")
        .env_remove("APP_DEPLOY_OPERATION_ID")
        .env_remove("APP_DEPLOY_GENERATION_ID")
        .env("APP_CLI_SKIP_PG_WAIT", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// 运行至退出（带硬超时防悬挂：失败路径都必须快速终结，不允许整窗等待）。
fn run_to_exit(mut command: Command, hard_timeout: Duration) -> Output {
    let mut child = command.spawn().expect("spawn app-cli");
    let deadline = Instant::now() + hard_timeout;
    loop {
        if child.try_wait().expect("try_wait app-cli").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "app-cli did not exit within {hard_timeout:?}; failure path must fail fast"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut output = child.wait_with_output().expect("collect app-cli output");
    // 拼接 stdout+stderr 便于断言（tracing 走 stderr、EVT 走 stdout）。
    output.stdout.extend_from_slice(&output.stderr);
    output
}

fn done_event_count(combined: &[u8]) -> usize {
    String::from_utf8_lossy(combined)
        .lines()
        .filter(|line| line.contains("APP-CLI-EVT") && line.contains("orchestration_done"))
        .count()
}

fn write_lock(workspace: &Path, content: &str) {
    std::fs::write(workspace.join("release.lock.toml"), content).expect("write lock");
}

fn temp_workspace() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("workspace tempdir");
    let workspace = dir.path().join("code");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    (dir, workspace)
}

/// A01：admin 端口被占 → legacy（无子命令）在 supervisor 编排之前 fail-fast：
/// 非零退出、不输出任何编排事件（无业务副作用）。
#[test]
fn legacy_admin_port_conflict_fails_fast_before_orchestration() {
    let (_dir, workspace) = temp_workspace();
    write_lock(&workspace, MINIMAL_LOCK);
    let (_hold, address) = reserve_port();
    let logs = workspace.parent().unwrap().join("logs");
    let output = run_to_exit(
        base_command(&workspace, &logs, &address),
        Duration::from_secs(30),
    );
    assert!(
        !output.status.success(),
        "bind conflict must exit non-zero; stdout+stderr: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        done_event_count(&output.stdout),
        0,
        "no orchestration must run when API bind fails"
    );
}

/// A01：admin 端口被占 → serve 形态同样在一切运行态副作用（cleanup/恢复/
/// stop_all）之前 fail-fast：非零退出、无编排事件。
#[test]
fn serve_admin_port_conflict_fails_fast_before_recovery() {
    let (_dir, workspace) = temp_workspace();
    write_lock(&workspace, MINIMAL_LOCK);
    let (_hold, address) = reserve_port();
    let logs = workspace.parent().unwrap().join("logs");
    let mut command = base_command(&workspace, &logs, &address);
    command.arg("serve");
    let output = run_to_exit(command, Duration::from_secs(30));
    assert!(
        !output.status.success(),
        "serve bind conflict must exit non-zero; stdout+stderr: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(done_event_count(&output.stdout), 0);
}

/// A04 早期失败：release.lock 损坏 → supervisor 加载失败，进程非零退出
/// （旧版 main 吞错退出码恒 0；P1-02 后退出码携带失败）。
#[test]
fn corrupted_lock_exits_non_zero() {
    let (_dir, workspace) = temp_workspace();
    write_lock(&workspace, "");
    let address = free_port_address();
    let logs = workspace.parent().unwrap().join("logs");
    let output = run_to_exit(
        base_command(&workspace, &logs, &address),
        Duration::from_secs(30),
    );
    assert!(
        !output.status.success(),
        "corrupted lock must exit non-zero; stdout+stderr: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    // 早期失败（spawn 之前）无 Done——由父进程退出监督兜底（P1-02 语义）。
    assert_eq!(done_event_count(&output.stdout), 0);
}

/// A02/A04 失败终局事件：pingap 编译失败（--pingap-bin /bin/false）→
/// 兜底路径补发**恰好一次** orchestration_done（含 orchestrator 自身条目与
/// 服务失败清单），进程非零退出。
#[test]
fn pingap_failure_emits_single_done_and_exits_non_zero() {
    let (_dir, workspace) = temp_workspace();
    write_lock(&workspace, MINIMAL_LOCK);
    let address = free_port_address();
    let logs = workspace.parent().unwrap().join("logs");
    let output = run_to_exit(
        base_command(&workspace, &logs, &address),
        Duration::from_secs(60),
    );
    let combined = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "pingap compile failure must exit non-zero; combined output: {combined}"
    );
    assert_eq!(
        done_event_count(&output.stdout),
        1,
        "failure Done must be emitted exactly once; combined output: {combined}"
    );
    let done_line = combined
        .lines()
        .find(|line| line.contains("orchestration_done"))
        .expect("done line");
    assert!(
        done_line.contains("orchestrator"),
        "orchestrator stage error must be reported in failed list: {done_line}"
    );
    // 服务 spawn 失败（node server.js 不存在）的容错清单也保留在同一 Done。
    assert!(
        done_line.contains("web"),
        "service-level failures must be preserved in the same Done: {done_line}"
    );
}
