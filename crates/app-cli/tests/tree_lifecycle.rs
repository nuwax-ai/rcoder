//! R01 验收反例：真实编排链的进程树收束（cross-platform.md §4 + review R01）。
//!
//! 反例构造：业务服务 root（tree-fixture serve）spawn **忽略 SIGTERM 且持有
//! 端口的孙进程**。停止必须收束整树——只杀直接 Child 或只发 TERM 都会让孙
//! 进程继续占端口，下一轮同名服务 bind 冲突。
//!
//! 两个真实链路用例（常驻 `serve` owner 的真实业务编排）：
//! 1. supervise 正常停机：SIGTERM → 宽限期 → 强杀整树（孙进程忽略 TERM，
//!    只有组级 SIGKILL 能收束）→ 干净退出 + 端口全部释放。
//! 2. 启动失败兜底：pingap 编译阶段失败 → shutdown_all 清理已启动的
//!    子进程（含孙进程）→ 端口全部释放 + 失败 Done，管理面仍在线。
//!
//! `run` 客户端保留管理 owner 的独立行为由 run_source_owner 覆盖。

#![cfg(unix)]

use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use std::net::TcpListener;

/// 业务服务端口 + 孙进程持有端口 + admin 端口一次预留（bind-0 后释放）。
fn reserve_ports() -> (u16, u16, String) {
    let pick = || {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    };
    let admin = TcpListener::bind("127.0.0.1:0").expect("reserve admin");
    let admin_addr = admin.local_addr().expect("addr").to_string();
    drop(admin);
    (pick(), pick(), admin_addr)
}

/// TCP connect 探测（127.0.0.1）。不用 bind 探测：std 在 Unix 默认开
/// SO_REUSEADDR，`0.0.0.0:P` 被占时 bind `127.0.0.1:P` 仍成功，探测失真。
fn port_connectable(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// 带 tree-fixture 服务（root + 忽略 TERM 孙进程）的 workspace。
fn fixture_workspace(root: &Path, svc_port: u16, gc_port: u16) -> std::path::PathBuf {
    let workspace = root.join("code");
    std::fs::create_dir_all(workspace.join("web")).expect("create workspace");
    let fixture = env!("CARGO_BIN_EXE_tree-fixture");
    let lock = format!(
        r#"
schema_version = 1
release_id = "r01-tree-test-0001"
workspace_name = "r01-tree"
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
port = {svc_port}

[services.run]
command = ["{fixture}", "serve", "{svc_port}", "{gc_port}"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 2

[services.health]
startup_path = "/health"
readiness_path = "/ready"
liveness_path = "/health"
startup_timeout_seconds = 5

[services.proxy]
path = "/api/web/"
strip_prefix = true

[[services.logs]]
id = "application"
glob = "web*.log*"
format = "jsonl"

[services.env]
TREE_FIXTURE_READY = "{}/grandchild-ready"
"#,
        root.display()
    );
    std::fs::write(workspace.join("release.lock.toml"), lock).expect("write lock");
    workspace
}

struct OwnedCli(Option<Child>);
impl Drop for OwnedCli {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take()
            && matches!(child.try_wait(), Ok(None))
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_cli(
    workspace: &Path,
    root: &Path,
    admin_addr: &str,
    pingap_bin: &str,
    skip_pingap_confirm: bool,
) -> OwnedCli {
    let mut command = Command::new(env!("CARGO_BIN_EXE_app-cli"));
    command
        .args([
            "serve",
            "--workspace",
            workspace.to_str().expect("workspace path"),
            "--log-dir",
            root.join("logs").to_str().expect("log path"),
            "--admin-addr",
            admin_addr,
            "--pingap-bin",
            pingap_bin,
        ])
        .env_remove("APP_DEPLOY_URL")
        .env_remove("APP_RELEASE_ID")
        .env_remove("APP_DEPLOY_OPERATION_ID")
        .env_remove("APP_DEPLOY_GENERATION_ID")
        .env("RUST_LOG", "app_cli=info")
        .env_remove("APP_CLI_ATTACH")
        .env("APP_CLI_STATE_ROOT", root.join("state"))
        .env("APP_CLI_REQUIRE_PG", "0")
        .env_remove("PGUSER")
        .env_remove("PGPASSWORD")
        .env("APP_CLI_SKIP_PG_WAIT", "1");
    if skip_pingap_confirm {
        // fake pingap 无 admin 通道：跳过初始配置确认（配置正确性由 -t 兜底）
        command.env("APP_CLI_SKIP_PINGAP_CONFIRM", "1");
    }
    // 默认 pingap 运行目录 /run/app-cli/pingap 是容器路径假设（N02 待修）；
    // 原生测试经 env 覆盖到临时目录
    command.env(
        "APP_CLI_PINGAP_RUNTIME_DIR",
        root.join("pingap-runtime").to_str().expect("path utf8"),
    );
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn app-cli");
    OwnedCli(Some(child))
}

/// Unix fake pingap：`-t` 校验模式立即成功；运行模式常驻（被 app-cli 受管
/// spawn，停止时随整树收束）。让成功路径走到 supervise 阶段。
fn write_fake_pingap(root: &Path) -> String {
    let path = root.join("fake-pingap");
    std::fs::write(
        &path,
        "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"-t\" ]; then exit 0; fi\ndone\nexec sleep 300\n",
    )
    .expect("write fake pingap");
    set_executable(&path);
    path.to_str().expect("path utf8").to_string()
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod");
}

/// 等端口可连接（孙进程/root 就绪信号）。
fn wait_port_taken(port: u16, deadline: Instant, what: &str) {
    loop {
        if port_connectable(port) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what} :{port} never became occupied"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 等端口不再可连接（持有者退出——比进程名扫描更强的释放证据）。
fn wait_port_released(port: u16, deadline: Instant, what: &str) {
    loop {
        if !port_connectable(port) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what} :{port} still occupied after stop — tree not converged"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 流式等 stderr 出现标记行（tracing 日志走 stderr；stdout 是 EVT 事件流）。
/// 拿走 stderr 管道逐行读——子进程树继承 stdout/stderr 管道，收集输出要等
/// 全部后代退出才有 EOF，这里只用有界预算找标记，不做全文收集。
fn wait_log_marker(child: &mut Child, marker: &str, budget: Duration) {
    use std::io::{BufRead, BufReader};
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + budget;
    let mut seen = String::new();
    loop {
        let line = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!("app-cli never logged {marker:?}: {error}; output:\n{seen}")
            })
            .expect("read stderr line");
        seen.push_str(&line);
        seen.push('\n');
        if line.contains(marker) {
            return;
        }
    }
}

/// 等进程退出并返回退出码（有界，不读管道）。
fn wait_exit(child: &mut Child, budget: Duration, what: &str) -> std::process::ExitStatus {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "{what} did not exit within {budget:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// R01 反例（正常停机）：孙进程忽略 TERM 且持端口，SIGTERM 停机后整树收束。
#[test]
fn supervised_stop_kills_term_ignoring_grandchild_and_releases_ports() {
    let root = tempfile::tempdir().expect("root tempdir");
    let (svc_port, gc_port, admin_addr) = reserve_ports();
    let workspace = fixture_workspace(root.path(), svc_port, gc_port);
    let pingap = write_fake_pingap(root.path());
    let mut cli = spawn_cli(&workspace, root.path(), &admin_addr, &pingap, true);

    // 编排启动：root 服务与孙进程先后占住各自端口
    let startup = Instant::now() + Duration::from_secs(20);
    wait_port_taken(gc_port, startup, "grandchild");
    wait_port_taken(svc_port, startup, "service root");
    // 启动阶段（supervise 安装信号处理前）TERM 是默认杀进程、不走优雅停机；
    // 等 stderr 出现 supervising 标记后再发信号，测的才是正常停机链。
    wait_log_marker(
        cli.0.as_mut().expect("owned child"),
        "supervising",
        Duration::from_secs(10),
    );

    // 正常停机信号（supervise select → shutdown_all）
    let pid = cli.0.as_mut().expect("owned child").id();
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(status.success());

    let child = cli.0.as_mut().expect("owned child");
    let exit = wait_exit(child, Duration::from_secs(20), "app-cli after SIGTERM");
    assert!(
        exit.success(),
        "clean shutdown must exit 0, got {exit:?} (grandchild cleanup must converge)"
    );

    // 整树收束证据：孙进程（忽略 TERM、持端口）与 root 的端口全部释放
    let release = Instant::now() + Duration::from_secs(5);
    wait_port_released(gc_port, release, "grandchild holder");
    wait_port_released(svc_port, release, "service root");

    // 第二轮同 workspace 立即可启动（新轮不能被残留进程卡住端口）
    let mut second = spawn_cli(&workspace, root.path(), &admin_addr, &pingap, true);
    let second_startup = Instant::now() + Duration::from_secs(20);
    wait_port_taken(gc_port, second_startup, "second round grandchild");
    wait_port_taken(svc_port, second_startup, "second round service");
    wait_log_marker(
        second.0.as_mut().expect("owned child"),
        "supervising",
        Duration::from_secs(10),
    );
    let second_pid = second.0.as_mut().expect("owned child").id();
    let _ = Command::new("kill")
        .args(["-TERM", &second_pid.to_string()])
        .status();
    let second_child = second.0.as_mut().expect("owned child");
    let second_exit = wait_exit(second_child, Duration::from_secs(20), "second round");
    assert!(
        second_exit.success(),
        "second round must also stop cleanly, got {second_exit:?}"
    );
    let second_release = Instant::now() + Duration::from_secs(5);
    wait_port_released(gc_port, second_release, "second round grandchild holder");
}

/// R01 反例（失败兜底）：pingap 编译失败 → 已启动服务整树清理，
/// 失败事件保留且管理面常驻，最后向捕获的 owner 发 SIGTERM 正常退出。
#[test]
fn startup_failure_cleanup_converges_service_tree() {
    let root = tempfile::tempdir().expect("root tempdir");
    let (svc_port, gc_port, admin_addr) = reserve_ports();
    let workspace = fixture_workspace(root.path(), svc_port, gc_port);
    let mut cli = spawn_cli(&workspace, root.path(), &admin_addr, "/bin/false", false);

    // The fixture acknowledges the real bind before its health endpoint is
    // available. Keep that durable evidence: a fast cleanup can release the
    // port before this test process is scheduled to observe it.
    let startup = Instant::now() + Duration::from_secs(20);
    let receipt = root.path().join("grandchild-ready");
    while !receipt.exists() {
        assert!(
            Instant::now() < startup,
            "grandchild never acknowledged bind"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let bound = std::fs::read_to_string(receipt).expect("grandchild bind evidence");
    assert_eq!(
        bound.split_whitespace().nth(1),
        Some(gc_port.to_string().as_str())
    );

    // Read the original failed Done from the real owner stream. The owner
    // must survive the business error; an exited client cannot prove this.
    use std::io::{BufRead, BufReader};
    let stdout = cli.0.as_mut().unwrap().stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    let failure = loop {
        let line = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("original failed Done before deadline")
            .expect("owner event line");
        if line.contains("orchestration_done") {
            break line;
        }
    };
    assert!(
        failure.contains("orchestrator"),
        "original startup error missing: {failure}"
    );
    assert!(
        cli.0.as_mut().unwrap().try_wait().unwrap().is_none(),
        "business failure must preserve the real management owner"
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let snapshot = runtime
        .block_on(runtime_supervisor::control(
            &root.path().join("state"),
            runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        ))
        .expect("management native status after business failure");
    assert_eq!(snapshot.binding.component, "app-cli");
    assert!(
        port_connectable(admin_addr.rsplit(':').next().unwrap().parse().unwrap()),
        "management HTTP listener must survive business failure"
    );

    // 兜底清理同样收束整树（孙进程忽略 TERM → 走强杀路径）
    let release = Instant::now() + Duration::from_secs(5);
    wait_port_released(gc_port, release, "grandchild holder after failure cleanup");
    wait_port_released(svc_port, release, "service root after failure cleanup");
    let child = cli.0.as_mut().unwrap();
    let signal = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(signal.success());
    assert!(
        wait_exit(
            child,
            Duration::from_secs(20),
            "management shutdown after failed business"
        )
        .success()
    );
}
