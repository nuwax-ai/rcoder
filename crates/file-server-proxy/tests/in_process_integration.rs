//! 进程内直连集成测试（feature `embed-file-server`，独立文件=独立进程——
//! 直连 router 是进程级全局，与 loopback 转发测试并跑会互相污染）。
//!
//! 铁证设计：**不启动任何 rust 上游监听**——rust_upstream_port 上无人听，
//! rust 域请求仍能 200（由进程内 router 直连服务）；存量路径走真实 TS 上游。
#![cfg(feature = "embed-file-server")]

use file_server_proxy::{
    FileServerProxyConfig, RoutePolicy, SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PROXY_PORT: u16 = 46100;
const RUST_UPSTREAM_PORT: u16 = 48186; // 故意不监听：直连的铁证
const TS_UPSTREAM_PORT: u16 = 46101;

async fn spawn_marker_upstream(port: u16, marker: &'static str) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind ts upstream");
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let _read = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{marker}",
                    marker.len()
                );
                let _written = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
}

async fn http_get(path: &str, headers: &[(&str, &str)]) -> String {
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", PROXY_PORT))
        .await
        .expect("connect proxy");
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    sock.write_all(req.as_bytes()).await.expect("send");
    let mut resp = String::new();
    sock.read_to_string(&mut resp).await.expect("recv");
    resp
}

/// 直连 router（marker）：任意路径回 200 + `in-process-router`。
fn marker_router() -> axum::Router {
    async fn marker() -> &'static str {
        "in-process-router"
    }
    axum::Router::new().fallback(axum::routing::any(marker))
}

#[tokio::test]
async fn in_process_router_serves_rust_domain_without_upstream_listener() {
    spawn_marker_upstream(TS_UPSTREAM_PORT, "upstream-ts").await;
    file_server_proxy::set_in_process_router(marker_router());

    // TsFirst：仅 /api/v1/userapp* → rust（直连）；存量路径 → TS
    file_server_proxy::init(FileServerProxyConfig {
        listen_port: PROXY_PORT,
        rust_upstream_port: RUST_UPSTREAM_PORT,
        ts_upstream_port: TS_UPSTREAM_PORT,
        policy: RoutePolicy::TsFirst,
    });
    file_server_proxy::try_start().await.expect("start");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // rust 域：上游端口无人听，仍由直连 router 服务（进程内 oneshot 铁证）
    let userapp = http_get("/api/v1/userapp/dev/start", &[]).await;
    assert!(
        userapp.contains("in-process-router"),
        "/api/v1/userapp/* 应由直连 router 服务: {userapp}"
    );

    // ts_first 语义：存量路径（含 userApp 标记）→ TS
    let legacy = http_get("/api/computer/get-file-list", &[]).await;
    assert!(legacy.contains("upstream-ts"), "存量路径应走 TS: {legacy}");
    let legacy_marked = http_get(
        "/api/computer/get-file-list",
        &[(SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP)],
    )
    .await;
    assert!(
        legacy_marked.contains("upstream-ts"),
        "ts_first 下 userApp 标记的存量路径也应走 TS: {legacy_marked}"
    );

    // 清除直连后回 loopback 转发路径：rust 域请求上游无人听 → 502（无直连兜底）
    file_server_proxy::clear_in_process_router();
    let after_clear = http_get("/api/v1/userapp/dev/start", &[]).await;
    assert!(
        after_clear.contains("502"),
        "清除直连后 rust 域应走上游转发（无人听=502）: {after_clear}"
    );

    file_server_proxy::stop().await.expect("stop");
}
