//! 分流反向代理集成测试：真实 pingora 代理 + 两个标记上游，
//! 验证 `x-service-type` header 决定业务归属（Rust/TS 上游）全链路。

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
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{marker}",
                    marker.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
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
async fn service_type_header_decides_upstream() {
    // macOS 沙箱下 rustls-native-certs 读 keychain 受限（pingora Server::new 初始化
    // 平台根证书失败）；指定 PEM 绕过。生产容器为 Linux（/etc/ssl/certs），不受影响。
    // 必须 earliest：在任何 Server::new 之前生效。
    // SAFETY: 测试启动早期、tokio worker 尚未并发读该 env，无实际竞争窗口。
    unsafe {
        if std::env::var("SSL_CERT_FILE").is_err() {
            std::env::set_var("SSL_CERT_FILE", "/etc/ssl/cert.pem");
        }
    }

    spawn_marker_upstream(RUST_UPSTREAM_PORT, "upstream-rust").await;
    spawn_marker_upstream(TS_UPSTREAM_PORT, "upstream-ts").await;

    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let proxy = tokio::spawn(async move {
        let cfg = FileServerProxyConfig {
            listen_port: PROXY_PORT,
            rust_upstream_port: RUST_UPSTREAM_PORT,
            ts_upstream_port: TS_UPSTREAM_PORT,
        };
        // 持有 sender 防止代理提前收到关闭信号
        let _hold = _shutdown_tx;
        file_server_proxy::run_file_server_proxy(cfg, shutdown_rx).await
    });
    // 等代理完成 bind 预检与 pingora 启动
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 同一路径, header 决定归属
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

    let _ = proxy.abort();
}
