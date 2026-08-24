//! 分流反向代理集成测试：真实 hyper 代理 + 两个标记上游，
//! 验证 `x-service-type` header 决定业务归属（Rust/TS 上游）全链路，
//! 以及 try_start/stop 生命周期（端口释放后可重启）。

use file_server_proxy::{FileServerProxyConfig, SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PROXY_PORT: u16 = 46000;
const RUST_UPSTREAM_PORT: u16 = 48086;
const TS_UPSTREAM_PORT: u16 = 46001;

/// 极简 HTTP 上游：收到请求即回 200 + 标识体（足以验证分流归属）。
async fn spawn_marker_upstream(port: u16, marker: &'static str) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind upstream");
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                if let Err(e) = sock.read(&mut buf).await {
                    eprintln!("marker upstream read error: {e}");
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{marker}",
                    marker.len()
                );
                if let Err(e) = sock.write_all(resp.as_bytes()).await {
                    eprintln!("marker upstream write error: {e}");
                }
            });
        }
    });
}

/// 手写 HTTP 客户端（不引入额外依赖），返回完整响应文本。
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

#[tokio::test]
async fn service_type_header_decides_upstream_and_lifecycle() {
    spawn_marker_upstream(RUST_UPSTREAM_PORT, "upstream-rust").await;
    spawn_marker_upstream(TS_UPSTREAM_PORT, "upstream-ts").await;

    file_server_proxy::init(FileServerProxyConfig {
        listen_port: PROXY_PORT,
        rust_upstream_port: RUST_UPSTREAM_PORT,
        ts_upstream_port: TS_UPSTREAM_PORT,
    });

    // ── 生命周期: start → status ──
    assert!(file_server_proxy::status().await.is_none(), "初始未运行");
    let addr = file_server_proxy::try_start().await.expect("start");
    assert!(addr.contains(&PROXY_PORT.to_string()));
    assert!(file_server_proxy::status().await.is_some(), "运行中");
    // 幂等: 重复 start 返回同一地址
    assert_eq!(
        file_server_proxy::try_start().await.expect("re-start"),
        addr
    );
    // 等代理 accept 循环就绪
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // ── 分流: 同一路径, header 决定归属 ──
    let path = "/api/computer/create-workspace";
    let no_header = http_get(path, &[]).await;
    assert!(
        no_header.contains("upstream-ts"),
        "无 header 应走 TS: {no_header}"
    );

    let userapp = http_get(path, &[(SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP)]).await;
    assert!(
        userapp.contains("upstream-rust"),
        "x-service-type: userapp 应走 Rust: {userapp}"
    );

    let computer = http_get(path, &[(SERVICE_TYPE_HEADER, "computer")]).await;
    assert!(
        computer.contains("upstream-ts"),
        "非 userapp 业务声明应走 TS: {computer}"
    );

    // health 探活走 TS（脚本健康检查依赖此行为）
    let health = http_get("/health", &[]).await;
    assert!(health.contains("upstream-ts"), "/health 应走 TS: {health}");

    // ── 生命周期: stop → 端口释放 → 可重启 ──
    file_server_proxy::stop().await.expect("stop");
    assert!(file_server_proxy::status().await.is_none(), "停止后无状态");
    // stop 返回时端口已释放: 立即重 bind 必须成功（外部服务如 TS 可立即占用 60000）
    let rebind = tokio::net::TcpListener::bind(("127.0.0.1", PROXY_PORT))
        .await
        .expect("stop 返回后端口应已释放");
    drop(rebind);

    // 重启后分流继续工作
    file_server_proxy::try_start()
        .await
        .expect("re-start after stop");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let again = http_get(path, &[(SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP)]).await;
    assert!(
        again.contains("upstream-rust"),
        "重启后 userapp 仍走 Rust: {again}"
    );

    file_server_proxy::stop().await.expect("final stop");
}
