//! Bin 级回归：API 预绑定 fail-fast；run 是显式操作客户端，真实输入
//! 或 pingap 失败仍非零退出、保留原失败 Done 和独立管理 owner。
//!
//! 走真实二进制（`CARGO_BIN_EXE_app-cli`）+ 受控 workspace / 端口 / pingap
//! 注入，不依赖 PG。预检失败用 /bin/false；启动后的终局保护使用支持
//! 完整预检、鉴权与应用结果的受控协议进程，不作为真实 Pingap 部署证据。
#![cfg(unix)]

use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

#[path = "fixtures/protocol_support.rs"]
mod protocol_support;

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

fn base_command(subcommand: &str, workspace: &Path, logs: &Path, admin_addr: &str) -> Command {
    base_command_with_proxy(
        subcommand,
        workspace,
        logs,
        admin_addr,
        Path::new("/bin/false"),
    )
}

fn base_command_with_proxy(
    subcommand: &str,
    workspace: &Path,
    logs: &Path,
    admin_addr: &str,
    proxy: &Path,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_app-cli"));
    command
        .args([
            subcommand,
            "--workspace",
            workspace.to_str().expect("workspace path"),
            "--log-dir",
            logs.to_str().expect("log path"),
            "--admin-addr",
            admin_addr,
            "--pingap-bin",
            proxy.to_str().expect("proxy fixture path"),
        ])
        .env_remove("APP_DEPLOY_URL")
        .env_remove("APP_RELEASE_ID")
        .env_remove("APP_DEPLOY_OPERATION_ID")
        .env_remove("APP_DEPLOY_GENERATION_ID")
        .env("APP_CLI_SKIP_PG_WAIT", "1")
        .env("APP_CLI_REQUIRE_PG", "0")
        .env_remove("APP_CLI_ATTACH")
        .env("PROJECT_ID", "bin-startup-fixture")
        .env(
            "APP_CLI_STATE_ROOT",
            workspace.parent().unwrap().join("state"),
        )
        .env(
            "APP_CLI_PINGAP_RUNTIME_DIR",
            workspace.parent().unwrap().join("pingap-runtime"),
        )
        .env_remove("PGUSER")
        .env_remove("PGPASSWORD")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// 运行至退出（带硬超时防悬挂：失败路径都必须快速终结，不允许整窗等待）。
fn run_to_exit(mut command: Command, hard_timeout: Duration) -> (Output, bool) {
    let state = command
        .get_envs()
        .find(|(key, _)| *key == "APP_CLI_STATE_ROOT")
        .and_then(|(_, value)| value)
        .map(std::path::PathBuf::from)
        .unwrap();
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
    // A successful bootstrap outlives its run client. Capture the real native
    // instance before shutdown; never kill a PID/name guessed from discovery.
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let native = runtime
        .block_on(runtime_supervisor::control(
            &state,
            runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        ))
        .ok();
    let management_alive = native.is_some()
        && runtime_supervisor::Owner::try_acquire(&state)
            .unwrap()
            .is_none();
    if let Some(native) = native {
        let mut request = runtime_supervisor::Request::new(runtime_supervisor::Action::Shutdown);
        request.capture_generation(native.generation.as_deref());
        runtime
            .block_on(runtime_supervisor::control_verified(
                &state,
                request,
                &native.supervisor_id,
            ))
            .expect("shutdown captured fixture management owner");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if runtime_supervisor::Owner::try_acquire(&state)
                .unwrap()
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fixture management owner failed to stop"
            );
            std::thread::sleep(Duration::from_millis(40));
        }
    }
    // Independent owner stdio is deliberately separate from the client pipe.
    // Preserve the real orchestration events for the failure assertions below:
    // 客户端桥接后管道已完整镜像编排事件（含终局 done）；仅当管道缺失终局
    // 信号时才并入 bootstrap 日志兜底（旧架构事件只在日志里），避免两路
    // 叠加把"Done 恰好一次"的断言翻倍。
    if done_event_count(&output.stdout) == 0 {
        for entry in std::fs::read_dir(state.parent().unwrap().join("logs"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("owner-bootstrap-")
            {
                output.stdout.extend(std::fs::read(entry.path()).unwrap());
            }
        }
    }
    (output, management_alive)
}

fn done_event_count(combined: &[u8]) -> usize {
    String::from_utf8_lossy(combined)
        .lines()
        .filter(|line| line.contains("APP-CLI-EVT") && line.contains("orchestration_done"))
        .count()
}

fn write_lock(workspace: &Path, content: &str) {
    std::fs::write(workspace.join("release.lock.toml"), content).expect("write lock");
    std::fs::create_dir_all(workspace.join("web")).unwrap();
    std::fs::write(
        workspace.join("workspace.manifest.toml"),
        "schema_version=1\n[workspace]\nname='bin-startup'\n",
    )
    .unwrap();
    std::fs::write(workspace.join("web/project.manifest.toml"),
        "schema_version=1\n[project]\nservice_id='web'\nname='Web'\ntype='node'\n[build]\ncommand=['true']\nartifact='unused.zip'\n[run]\ncommand=['node','server.js']\n[health]\nstartup_timeout_seconds=2\n").unwrap();
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
    let (output, management_alive) = run_to_exit(
        base_command("run", &workspace, &logs, &address),
        Duration::from_secs(30),
    );
    assert!(
        !output.status.success(),
        "bind conflict must exit non-zero; stdout+stderr: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !management_alive,
        "bind failure cannot leave a management owner"
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
    let command = base_command("serve", &workspace, &logs, &address);
    let (output, management_alive) = run_to_exit(command, Duration::from_secs(30));
    assert!(
        !output.status.success(),
        "serve bind conflict must exit non-zero; stdout+stderr: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(!management_alive);
    assert_eq!(done_event_count(&output.stdout), 0);
}

/// 损坏派生 lock 被当前 Source 重建；真实 pingap 失败使客户端非零退出，
/// 管理 owner 保持在线。完整成功重试由 run_source_owner 覆盖。
#[test]
fn corrupted_lock_is_rebuilt_and_real_startup_failure_exits_non_zero() {
    let (_dir, workspace) = temp_workspace();
    write_lock(&workspace, "");
    let address = free_port_address();
    let logs = workspace.parent().unwrap().join("logs");
    let (output, management_alive) = run_to_exit(
        base_command("run", &workspace, &logs, &address),
        Duration::from_secs(30),
    );
    assert!(
        !output.status.success(),
        "real startup failure must exit non-zero; stdout+stderr: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        management_alive,
        "failed Source cannot terminate its management owner"
    );
    assert!(
        app_cli::manifest::read_release_lock(&workspace).is_ok(),
        "corrupt derived lock must be rebuilt"
    );
    assert_eq!(done_event_count(&output.stdout), 1);
}

/// A02/A04：配置完整预检通过，真实协议进程启动后拒绝本次 publication。
/// 终局保留 orchestrator 与真实服务 spawn 失败，Done 恰好一次、客户端
/// 非零退出；管理 owner 保留且最终精确清理代理。
#[test]
fn pingap_failure_emits_single_done_and_exits_non_zero() {
    let (directory, workspace) = temp_workspace();
    write_lock(&workspace, MINIMAL_LOCK);
    std::fs::write(workspace.join("workspace.manifest.toml"), "schema_version=1\n[workspace]\nname='bin-startup'\n[pingap]\nmode='custom'\nconfig='protocol-listener.toml'\n").unwrap();
    protocol_support::write_listener_override(&workspace, "web");
    let missing_service = directory.path().join("intentionally-missing-service");
    std::fs::write(workspace.join("web/project.manifest.toml"), format!("schema_version=1\n[project]\nservice_id='web'\nname='Web'\ntype='node'\n[build]\ncommand=['true']\nartifact='unused.zip'\n[run]\ncommand=[{:?}]\nshutdown_timeout_seconds=1\n[health]\nstartup_timeout_seconds=2\n[proxy]\npath='/'\n", missing_service.to_string_lossy())).unwrap();
    let proxy = protocol_support::write_pingap(directory.path());
    let executable = env!("CARGO_BIN_EXE_tree-fixture").replace('\'', "'\"'\"'");
    // The witness is written only for a real serving launch, never for either
    // preflight command. Unix exec preserves this exact child PID.
    std::fs::write(&proxy,format!("#!/bin/sh\nfor arg in \"$@\"; do\ncase \"$arg\" in --apply-protocol-version|-t) exec '{executable}' protocol-pingap \"$@\";; esac\ndone\nprintf '%s\\n' \"$$\" > \"$PROTOCOL_PROXY_STARTED_FILE\"\nexec '{executable}' protocol-pingap \"$@\"\n")).unwrap();
    let fault = directory.path().join("proxy-admin-fault");
    std::fs::write(&fault, "failed").unwrap();
    let started = directory.path().join("proxy-started-pid");
    let proxy_identity = directory.path().join("proxy-identity.json");
    let address = free_port_address();
    let logs = workspace.parent().unwrap().join("logs");
    let mut command = base_command_with_proxy("run", &workspace, &logs, &address, &proxy);
    command
        .env("PROTOCOL_PROXY_ADMIN_FAULT", &fault)
        .env("PROTOCOL_PROXY_STARTED_FILE", &started)
        .env("PROTOCOL_PROXY_PID_FILE", &proxy_identity)
        .env(
            "APP_CLI_PINGAP_ADMIN_PORT",
            protocol_support::reserve_port().to_string(),
        )
        .env(
            "APP_CLI_SUPERVISOR_SOCKET",
            directory.path().join("no-supervisor.sock"),
        );
    let (output, management_alive) = run_to_exit(command, Duration::from_secs(60));
    assert!(
        management_alive,
        "business failure cannot terminate management"
    );
    let combined = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "post-spawn protocol failure must exit non-zero; combined output: {combined}"
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
    let event: serde_json::Value = serde_json::from_str(
        done_line
            .split_once("APP-CLI-EVT ")
            .expect("real orchestration event prefix")
            .1,
    )
    .unwrap();
    let failed = event["failed"].as_array().expect("structured failure list");
    assert_eq!(
        failed.len(),
        2,
        "one service spawn failure and one publication failure must be retained: {done_line}"
    );
    let service_failure = failed
        .iter()
        .find(|entry| entry["service"] == "web")
        .expect("web failure entry");
    assert!(
        service_failure["error"]
            .as_str()
            .unwrap()
            .contains("intentionally-missing-service"),
        "the service failure must be the real missing executable: {done_line}"
    );
    let publication_failure = failed
        .iter()
        .find(|entry| entry["service"] == "orchestrator")
        .expect("orchestrator failure entry");
    assert!(
        publication_failure["error"]
            .as_str()
            .unwrap()
            .contains("controlled post-spawn application failure"),
        "the terminal must retain the application protocol rejection: {done_line}"
    );
    assert!(
        combined.contains("controlled post-spawn application failure"),
        "the compiled candidate must reach the actual admin failure: {combined}"
    );
    let proxy_pid: u32 = std::fs::read_to_string(&started)
        .expect("proxy was really launched after preflight")
        .trim()
        .parse()
        .unwrap();
    assert!(proxy_pid > 0);
    if let Ok(bytes) = std::fs::read(&proxy_identity) {
        let identity: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(identity["pid"].as_u64(), Some(u64::from(proxy_pid)));
    }
    assert!(
        std::net::TcpStream::connect("127.0.0.1:9080").is_err(),
        "captured owner shutdown must leave no public proxy listener"
    );
    // An intentionally missing executable deterministically fails service spawn.
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
    let mut command = base_command("run", &workspace, &logs, &address);
    // 注入部署三元组：deploy_requested() = true（旧版会进入 deploy_stage）
    command
        .env("APP_DEPLOY_URL", "http://127.0.0.1:1/nonexistent.zip")
        .env("APP_RELEASE_ID", "test-release")
        .env("APP_DEPLOY_GENERATION_ID", "test-gen");
    let volume_root = workspace.parent().unwrap();
    let (output, management_alive) = run_to_exit(command, Duration::from_secs(30));
    assert!(!management_alive);
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
    let mut command = base_command("serve", &workspace, &logs, &address);
    command.arg("--attach");
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
    let mut command = base_command("serve", &workspace, &logs, &server_addr);
    command.arg("--attach");
    let (output, management_alive) = run_to_exit(command, Duration::from_secs(15));
    assert!(!management_alive);
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

    let mut command_a = base_command("serve", &workspace, &logs, &address);
    let mut command_b = base_command("serve", &workspace, &logs, &address);
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
