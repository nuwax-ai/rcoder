//! Windows 原生生命周期集成测试（XP 生命周期门禁的真机等价物）。
//!
//! 场景：空 workspace 的 app-cli（无子命令）→ idle 形态常驻：
//! /health 200 应答 → 强杀（taskkill /F）→ 进程退出、端口释放。
//! 对照 Unix 侧 serve_restart/bin_startup（#![cfg(unix)]）——本文件补
//! Windows 侧的最小真实生命周期闭环。
#![cfg(windows)]

use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
            "--workspace",
            workspace.to_str().expect("workspace path"),
            "--log-dir",
            logs.to_str().expect("log path"),
            "--admin-addr",
            &address,
        ])
        .env_remove("APP_DEPLOY_URL")
        .env_remove("APP_RELEASE_ID")
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
    let killed = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/F", "/T"])
        .output()
        .expect("taskkill");
    assert!(
        killed.status.success(),
        "taskkill must succeed: {}",
        String::from_utf8_lossy(&killed.stderr)
    );
    child.wait().expect("reap app-cli");

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
