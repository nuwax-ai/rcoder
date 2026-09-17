//! R01 验收 fixture：受控进程树（仅测试支持目标，不属于生产二进制）。
//!
//! 角色：
//! - `hold <port> --ignore-term`：占住 TCP 端口的常驻进程（孙进程角色）。
//!   `--ignore-term` 安装 SIGTERM 忽略——验证 app-cli 停止链在宽限期后
//!   强杀整树（SIGKILL 进程组 / TerminateJobObject）而非只发 TERM。
//! - `serve <svc_port> <gc_port>`：业务服务 root。先 spawn 自身
//!   `hold <gc_port> --ignore-term`（对 app-cli 是孙进程），再以最小
//!   HTTP 服务常驻（readiness 探测 200），接受连接直到被停止。
//!
//! 全部 std 实现、无平台 API 依赖（除 Unix SIGTERM 忽略一处）。

// 仅测试 fixture 允许 unsafe：SIG_IGN 无 std API（生产代码不受此豁免）。
#![allow(unsafe_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("hold") => {
            let port: u16 = args[2].parse().expect("hold <port>");
            let ignore_term = args.iter().any(|arg| arg == "--ignore-term");
            hold_port(port, ignore_term);
        }
        Some("serve") => {
            let svc_port: u16 = args[2].parse().expect("serve <svc_port> <gc_port>");
            let gc_port: u16 = args[3].parse().expect("serve <svc_port> <gc_port>");
            serve_with_grandchild(svc_port, gc_port);
        }
        _ => {
            eprintln!(
                "usage: tree-fixture hold <port> [--ignore-term] | serve <svc_port> <gc_port>"
            );
            std::process::exit(2);
        }
    }
}

/// 占住端口（孙进程）：接受连接后立即挂起，不做任何事。
fn hold_port(port: u16, ignore_term: bool) {
    if ignore_term {
        ignore_sigterm();
    }
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("tree-fixture hold: bind {port} failed: {error}");
            std::process::exit(3);
        }
    };
    for stream in listener.incoming() {
        // 只消费连接，不响应——纯端口持有者
        drop(stream);
    }
}

/// 业务服务 root：spawn 忽略 TERM 的孙进程（持 gc_port），自身最小 HTTP 常驻。
fn serve_with_grandchild(svc_port: u16, gc_port: u16) {
    let exe = std::env::current_exe().expect("fixture path");
    let mut grandchild = Command::new(exe)
        .args(["hold", &gc_port.to_string(), "--ignore-term"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn grandchild holder");
    let listener = match TcpListener::bind(("0.0.0.0", svc_port)) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("tree-fixture serve: bind {svc_port} failed: {error}");
            let _ = grandchild.kill();
            let _ = grandchild.wait();
            std::process::exit(3);
        }
    };
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        // 最小 HTTP 应答（readiness 探测 200）
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    }
}

#[cfg(unix)]
fn ignore_sigterm() {
    // SIG_IGN 跨 exec 保留；无 std API，经 libc 安装（仅本测试 fixture）
    unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
}

#[cfg(windows)]
fn ignore_sigterm() {
    // Windows 停止链无优雅信号概念（Job Terminate 即强杀），
    // ignore-term 标记在该平台为 no-op。
}
