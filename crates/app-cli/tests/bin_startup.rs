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
version = "0.14.3"
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

/// P1-01 强化：admin 端口被占 + APP_DEPLOY_URL 设置（deploy_requested=true）
/// → legacy 形态在 deploy_stage 之前 fail-fast：非零退出、无任何部署副作用
/// （不创建 .incoming/.staging/.deploy-state.toml）。
///
/// 旧版 main.rs 的执行顺序是 deploy_stage → bind，导致 deploy_stage 先于
/// 端口检查执行并可能修改运行目录。修复后 bind → deploy_stage，端口冲突
/// 在一切副作用前 fail-fast。
#[test]
fn legacy_deploy_url_with_port_conflict_has_no_deploy_side_effects() {
    let (_dir, workspace) = temp_workspace();
    write_lock(&workspace, MINIMAL_LOCK);
    let (_hold, address) = reserve_port();
    let logs = workspace.parent().unwrap().join("logs");
    let mut command = base_command(&workspace, &logs, &address);
    // 注入部署三元组：deploy_requested() = true（旧版会进入 deploy_stage）
    command
        .env("APP_DEPLOY_URL", "http://127.0.0.1:1/nonexistent.zip")
        .env("APP_RELEASE_ID", "test-release")
        .env("APP_DEPLOY_GENERATION_ID", "test-gen");
    let volume_root = workspace.parent().unwrap();
    let output = run_to_exit(command, Duration::from_secs(30));
    assert!(
        !output.status.success(),
        "deploy URL + bind conflict must exit non-zero; stdout+stderr: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    // 关键断言：无部署副作用——.incoming/.staging/.deploy-state.toml 不应存在
    assert!(
        !volume_root.join(".incoming").exists(),
        "deploy stage must not create .incoming when API bind fails first"
    );
    assert!(
        !volume_root.join(".staging").exists(),
        "deploy stage must not create .staging when API bind fails first"
    );
    assert!(
        !volume_root.join(".deploy-state.toml").exists(),
        "deploy stage must not create .deploy-state.toml when API bind fails first"
    );
    assert_eq!(
        done_event_count(&output.stdout),
        0,
        "no orchestration must run when API bind fails (deploy URL + port conflict)"
    );
}

/// P2-08：serve --attach 无已有实例 → 端口空闲时正常绑定（进程存活 = 进入 serve 流程）。
#[test]
fn serve_attach_without_existing_instance_starts_as_owner() {
    let (_dir, workspace) = temp_workspace();
    write_lock(&workspace, MINIMAL_LOCK);
    let address = free_port_address();
    let logs = workspace.parent().unwrap().join("logs");
    let mut command = base_command(&workspace, &logs, &address);
    command.arg("--attach").arg("serve");
    let mut child = command.spawn().expect("spawn attach serve");
    // 等待足够时间让进程完成 attach 检测（无实例 → serve_without_attach）
    std::thread::sleep(std::time::Duration::from_secs(3));
    // 进程应该仍然存活（进入了 serve 流程，不会立即因 attach 失败退出）
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "attach without existing instance should proceed to serve, not exit immediately"
    );
    let _ = child.kill();
    let _ = child.wait();
}

/// P2-08：serve --attach 身份不匹配 → 立即退出（exit 1）。
/// 使用简单 HTTP 服务器返回不匹配的 identity。
#[test]
fn serve_attach_identity_mismatch_exits_fast() {
    // 启动一个简单 HTTP 服务器模拟已有实例（返回不匹配的 identity）
    let server_addr = free_port_address();
    let server_bind = server_addr.clone();
    let _server_handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let app = axum::Router::new().route(
                "/v1/runtime/identity",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "success": true,
                        "data": {
                            "application_id": "remote-app",
                            "workspace_id": "remote-workspace"
                        }
                    }))
                }),
            );
            let listener = tokio::net::TcpListener::bind(&server_bind).await.unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    std::thread::sleep(std::time::Duration::from_millis(500));

    let (_dir, workspace) = temp_workspace();
    write_lock(&workspace, MINIMAL_LOCK);
    let logs = workspace.parent().unwrap().join("logs");
    let mut command = base_command(&workspace, &logs, &server_addr);
    command.arg("--attach").arg("serve");
    let output = run_to_exit(command, Duration::from_secs(15));
    assert!(
        !output.status.success(),
        "attach with mismatched identity must exit non-zero; stdout+stderr: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let combined = String::from_utf8_lossy(&output.stdout);
    assert!(
        combined.contains("identity mismatch") || combined.contains("refusing to attach"),
        "error should mention identity mismatch; output: {combined}"
    );
}

/// XP01（Unix）：两个 CLI 并发首次启动（真实二进制同端口同 workspace）——
/// 恰好一个 owner 胜出（/health 200 常驻），另一个非零退出，无第二业务树。
#[test]
fn xp01_two_cli_concurrent_first_start_single_winner() {
    let (_dir, workspace) = temp_workspace();
    let address = free_port_address();
    let logs = workspace.parent().unwrap().join("logs");

    let mut command_a = base_command(&workspace, &logs, &address);
    command_a.arg("serve");
    let mut command_b = base_command(&workspace, &logs, &address);
    command_b.arg("serve");
    let mut first = command_a.spawn().expect("spawn first serve");
    let mut second = command_b.spawn().expect("spawn second serve");

    // 收敛判定（30s 上限）：胜者 /health 200 且存活；败者已退出且非零
    // （OwnerGuard 排他使后到者在任何运行态副作用前失败）
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "concurrent start must converge quickly"
        );
        let first_alive = first.try_wait().unwrap().is_none();
        let second_status = second.try_wait().unwrap();

        let first_healthy = first_alive && http_status(&address) == Some(200);
        let second_healthy = second_status.is_none() && http_status(&address) == Some(200);
        let _ = second_healthy; // 端口单占：健康应答只能来自存活者之一

        if first_healthy && let Some(status) = second_status {
            assert!(
                !status.success(),
                "loser must exit non-zero (owner lock / bind conflict)"
            );
            let _ = first.kill();
            let _ = first.wait();
            return;
        }
        if second_healthy && !first_alive {
            // 反向胜出：second 存活健康、first 已退出非零
            let status = first.wait().unwrap();
            assert!(!status.success(), "loser must exit non-zero");
            let _ = second.kill();
            let _ = second.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 原生 TCP 上的最小 HTTP/1.1 GET /health 状态码探测。
fn http_status(address: &str) -> Option<u16> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(address).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .write_all(
            format!("GET /health HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    String::from_utf8_lossy(&response)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
}
