//! desktop 的 rcoder 服务嵌入：自建 tokio runtime 上跑 `rcoder::run()`
//! 全量组合（引擎 + HTTP listener + Pingora），关窗发 SIGTERM 语义的
//! shutdown 由 OS 信号路径兜底（骨架阶段；完整优雅关停后续接
//! broadcast 通道）。运行时形态/健康探活经主端口 `/health`。

use std::time::Duration;

/// 主端口的健康探查（纯 std TCP + 原始 HTTP 请求——gpui executor 非 tokio，
/// 不能用 reqwest；同步实现经 background executor 调用）。
pub fn tcp_health_probe(port: u16) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(1))
        .map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(format!("GET /health HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut buf = [0u8; 128];
    let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
    let head = String::from_utf8_lossy(&buf[..n]);
    if head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        Err(format!("non-200 status line: {}", head.lines().next().unwrap_or_default()))
    }
}

/// 在独立线程上启动完整 rcoder 服务（自建多线程 tokio runtime）。
///
/// 返回 join handle（骨架阶段不做窗口侧取消；服务随进程退出）。
pub fn spawn_rcoder_service() -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("rcoder-service".to_owned())
        .spawn(|| {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("[desktop] rcoder service runtime build failed: {error}");
                    return;
                }
            };
            if let Err(error) = runtime.block_on(rcoder::run()) {
                eprintln!("[desktop] rcoder service exited with error: {error:#}");
            }
        })
        .expect("spawn rcoder service thread")
}
