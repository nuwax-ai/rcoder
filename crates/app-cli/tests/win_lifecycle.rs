//! Windows 原生生命周期集成测试（XP 生命周期门禁的真机等价物）。
//!
//! 场景：空 workspace 的 app-cli serve --control-only 常驻：
//! /health 200 应答 → 强杀（taskkill /F）→ 进程退出、端口释放。
//! 对照 Unix 侧 serve_restart/bin_startup（#![cfg(unix)]）——本文件补
//! Windows 侧的最小真实生命周期闭环。
#![cfg(windows)]

use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// 结束 app-cli 及其子进程并回收它。`taskkill /T` 的退出码不可靠：它先枚举进程树再
/// 逐个结束，轮到某个子进程时那个进程可能已经退出，这时它会报错；它报告成功时，目标
/// 进程可能还没完全退出。所以只看目标进程是否真的结束：没结束就再试，最后用进程句柄结束。
fn kill_tree(child: &mut Child) {
    for _ in 0..3 {
        let _killed = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/F", "/T"])
            .output();
        if child.try_wait().expect("observe app-cli").is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _killed = child.kill();
    let _reaped = child.wait();
}

fn free_port_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
    let address = listener.local_addr().expect("addr").to_string();
    drop(listener);
    address
}

fn http_get_health(addr: &str) -> Option<u16> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    use std::io::{Read, Write};
    stream
        .write_all(
            format!("GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    let text = String::from_utf8_lossy(&response);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
}

/// 空容器 idle 生命周期：spawn → /health 200 → taskkill → 端口释放。
#[test]
fn windows_idle_serve_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("code");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let address = free_port_address();
    let logs = dir.path().join("logs");

    let mut child = Command::new(env!("CARGO_BIN_EXE_app-cli"))
        .args([
            "serve",
            "--control-only",
            "--workspace",
            workspace.to_str().expect("workspace path"),
            "--log-dir",
            logs.to_str().expect("log path"),
            "--admin-addr",
            &address,
        ])
        .env_remove("APP_DEPLOY_URL")
        .env_remove("APP_RELEASE_ID")
        .env_remove("APP_DEPLOY_OPERATION_ID")
        .env_remove("APP_CLI_ATTACH")
        .env("APP_CLI_STATE_ROOT", dir.path().join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn app-cli idle");

    // /health 200（有界等待 API 起来）
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut healthy = false;
    while Instant::now() < deadline {
        if http_get_health(&address) == Some(200) {
            healthy = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(healthy, "idle app-cli must answer /health 200 at {address}");

    // 强杀（容器终止等价物）
    kill_tree(&mut child);

    // 端口随进程终止释放（可重新绑定）
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::net::TcpListener::bind(&address).is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "port {address} must be released after process termination"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// XP01（Windows）：两个 CLI 并发首次启动（真实二进制同端口同 workspace）——
/// 恰好一个 owner 胜出（/health 200 常驻），另一个复用管理面后成功退出。
#[test]
fn xp01_two_cli_concurrent_first_start_single_winner() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workspace = dir.path().join("code");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let address = free_port_address();
    let logs = dir.path().join("logs");

    let spawn_serve = || {
        Command::new(env!("CARGO_BIN_EXE_app-cli"))
            .args([
                "serve",
                "--control-only",
                "--workspace",
                workspace.to_str().expect("workspace path"),
                "--log-dir",
                logs.to_str().expect("log path"),
                "--admin-addr",
                &address,
            ])
            .env_remove("APP_DEPLOY_URL")
            .env_remove("APP_RELEASE_ID")
            .env_remove("APP_DEPLOY_OPERATION_ID")
            .env_remove("APP_CLI_ATTACH")
            .env("APP_CLI_STATE_ROOT", dir.path().join("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn serve")
    };
    let mut first = spawn_serve();
    let mut second = spawn_serve();

    // 收敛判定：胜者 /health 200 且存活；后到者已成功复用（真实排他锁仍只有一个 owner）
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            Instant::now() < deadline,
            "concurrent start must converge quickly"
        );
        let first_alive = first.try_wait().expect("try_wait first").is_none();
        let second_status = second.try_wait().expect("try_wait second");

        if http_get_health(&address) == Some(200)
            && first_alive
            && let Some(status) = second_status
        {
            assert!(
                status.success(),
                "control-only caller must reuse the verified owner"
            );
            kill_tree(&mut first);
            return;
        }
        if http_get_health(&address) == Some(200) && second_status.is_none() && !first_alive {
            let status = first.wait().expect("reap first");
            assert!(
                status.success(),
                "control-only caller must reuse the verified owner"
            );
            kill_tree(&mut second);
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
