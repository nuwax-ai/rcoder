//! gRPC 服务工具函数

use std::path::Path;
use std::time::Duration;

use shared_types::{NOVNC_PORT, XVNC_RFB_PORT};
use shared_types_i18n::get_i18n_message;

pub async fn check_port_available(port: u16, timeout_millis: u64) -> bool {
    use tokio::net::TcpStream;

    matches!(
        tokio::time::timeout(
            Duration::from_millis(timeout_millis),
            TcpStream::connect(format!("127.0.0.1:{}", port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// 检查 noVNC WebSocket 代理是否真正可用
///
/// 使用 tokio-tungstenite 库进行标准 WebSocket 握手验证
/// 比仅检查端口更可靠（端口可达 ≠ WebSocket 代理可用）
///
/// # 参数
/// - `port`: noVNC 监听端口（通常为 6080）
/// - `timeout_millis`: 超时时间（毫秒）
///
/// # 返回
/// - `true`: WebSocket 握手成功（返回 101 Switching Protocols）
/// - `false`: 握手失败或超时
pub async fn check_novnc_websocket_ready(port: u16, timeout_millis: u64) -> bool {
    use tokio_tungstenite::connect_async;

    let url = format!("ws://127.0.0.1:{}/websockify", port);

    match tokio::time::timeout(Duration::from_millis(timeout_millis), connect_async(&url)).await {
        Ok(Ok((mut ws_stream, response))) => {
            // WebSocket 握手成功，检查状态码
            let status = response.status();
            // 主动关闭连接（仅用于检查，不需要保持）
            let _ = ws_stream.close(None).await;
            status == 101
        }
        Ok(Err(e)) => {
            tracing::debug!("WebSocket handshake failed: {}", e);
            false
        }
        Err(_) => {
            tracing::debug!("WebSocket handshake timed out after {}ms", timeout_millis);
            false
        }
    }
}

/// RFB 协议版本串长度（VNC server 接受连接后主动发送）
///
/// 格式为 `RFB 003.008\n` 共 12 字节（RFC 6143 §7.1.1）。
/// 不同 Xvnc 版本可能为 007/009，前缀恒为 `RFB `。
const RFB_PROTOCOL_VERSION_LEN: usize = 12;

/// 检查 Xvnc RFB 后端是否真正可服务
///
/// 与 `check_port_available` / `check_novnc_websocket_ready` 的本质区别：
/// RFB 是 **server-first** 协议——Xvnc 接受 TCP 连接后会主动发送 12 字节协议版本串
/// （`RFB 003.00x\n`）。读到该串才能证明 Xvnc 的事件循环在正常处理连接，
/// 而非仅内核 listen queue 还在（TCP 通但进程卡死/僵尸）。
///
/// 这是穿透「Xvnc 卡死/僵尸但 5900 listen socket 仍在」盲区的唯一可靠手段，
/// 也是 websockify 报 `Target closed`（连不上后端 5900）的根因探测点。
///
/// # 参数
/// - `port`: Xvnc RFB 监听端口（通常为 `shared_types::XVNC_RFB_PORT` = 5900）
/// - `timeout_millis`: 整个探测（TCP connect + RFB read）的总预算
///
/// # 返回
/// - `true`: 读到 12 字节且以 `RFB ` 开头
/// - `false`: 连接失败 / 超时 / 读到的字节不是合法 RFB 版本串
pub async fn check_vnc_rfb_ready(port: u16, timeout_millis: u64) -> bool {
    match tokio::time::timeout(
        Duration::from_millis(timeout_millis),
        check_vnc_rfb_ready_inner(port),
    )
    .await
    {
        Ok(Ok(true)) => true,
        Ok(Ok(false)) => {
            tracing::debug!("RFB probe read non-RFB bytes from port {}", port);
            false
        }
        Ok(Err(e)) => {
            tracing::debug!("RFB probe I/O error on port {}: {}", port, e);
            false
        }
        Err(_) => {
            tracing::debug!(
                "RFB probe timed out after {}ms on port {}",
                timeout_millis,
                port
            );
            false
        }
    }
}

async fn check_vnc_rfb_ready_inner(port: u16) -> std::io::Result<bool> {
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpStream;

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).await?;

    let mut buf = [0u8; RFB_PROTOCOL_VERSION_LEN];
    // read_exact 要求正好读满 12 字节；Xvnc 健康时一定发 12 字节，
    // 不足或对端提前断流（UnexpectedEof）会返回 Err → 外层判 false。
    stream.read_exact(&mut buf).await?;

    // 主动关闭，不留半连接。忽略关闭错误（对端已 RST 也无所谓）。
    let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;

    // 校验前缀 `RFB `（不校验具体版本号，兼容 003.007/008/009）
    Ok(buf.starts_with(b"RFB "))
}

/// VNC 探测结果
///
/// 抽取自 `get_vnc_status` 的核心探测逻辑，供 gRPC（GetVncStatus）和 HTTP
/// （`/computer/agent/vnc-status`）共用，保证两者 vnc_ready/novnc_ready/message 语义一致。
#[derive(Debug, Clone)]
pub struct VncProbeResult {
    /// VNC 全链路就绪（novnc_ready && 5900 RFB）
    pub vnc_ready: bool,
    /// noVNC 前端代理层就绪（文件标记 + 6080 端口 + WebSocket 升级）
    pub novnc_ready: bool,

    // 以下为诊断明细，供日志输出
    pub novnc_port_ready: bool,
    pub novnc_websocket_ready: bool,
    pub rfb_ready: bool,

    /// i18n 状态描述消息
    pub message: String,
}

/// 探测 VNC 服务就绪状态（noVNC 前端层 + Xvnc RFB 后端层）
///
/// 封装「文件标记 + 6080 TCP + 6080 WebSocket + 5900 RFB」四层探测与 message 分支。
/// 调用方（gRPC / HTTP）只需补充各自上下文特有的 `uptime_seconds` / `container_id`。
///
/// - `timeout_millis`: 每层探测的预算（复用 `port_check_timeout_millis`）
/// - `locale`: i18n 语言（gRPC/HTTP 两端的 `locale_from_*` 均返回 `&'static str`）
pub async fn probe_vnc_readiness(timeout_millis: u64, locale: &str) -> VncProbeResult {
    let file_exists = Path::new("/tmp/vnc_ready").exists();

    let novnc_port_ready = check_port_available(NOVNC_PORT, timeout_millis).await;
    let novnc_websocket_ready = if novnc_port_ready {
        check_novnc_websocket_ready(NOVNC_PORT, timeout_millis).await
    } else {
        false
    };
    let rfb_ready = check_vnc_rfb_ready(XVNC_RFB_PORT, timeout_millis).await;

    let novnc_ready = file_exists && novnc_port_ready && novnc_websocket_ready;
    let vnc_ready = novnc_ready && rfb_ready;

    let message = if vnc_ready {
        get_i18n_message("grpc.status.vnc_ready", locale)
    } else if !file_exists {
        get_i18n_message("grpc.status.vnc_not_ready", locale)
    } else if novnc_ready && !rfb_ready {
        get_i18n_message("grpc.status.vnc_backend_not_ready", locale)
    } else if !novnc_websocket_ready {
        // noVNC 前端 WebSocket 不可用（6080 端口不通，或端口通但 WS 握手失败）
        get_i18n_message("grpc.status.vnc_port_unreachable", locale)
    } else {
        get_i18n_message("grpc.status.vnc_not_ready", locale)
    };

    VncProbeResult {
        vnc_ready,
        novnc_ready,
        novnc_port_ready,
        novnc_websocket_ready,
        rfb_ready,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rfb_ready_returns_true_on_valid_version_string() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(b"RFB 003.008\n").await;
            }
        });

        assert!(check_vnc_rfb_ready(port, 2000).await);
    }

    #[tokio::test]
    async fn rfb_ready_returns_false_on_garbage_bytes() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\n").await;
            }
        });

        assert!(!check_vnc_rfb_ready(port, 2000).await);
    }

    #[tokio::test]
    async fn rfb_ready_returns_false_on_accept_then_eof() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                drop(sock); // accept 后立即关闭，模拟 Xvnc 崩溃
            }
        });

        assert!(!check_vnc_rfb_ready(port, 2000).await);
    }

    #[tokio::test]
    async fn rfb_ready_returns_false_on_frozen_accept() {
        // ⭐ 本次 bug 根因场景：Xvnc accept 成功但永不发数据（卡死）。
        // read_exact 必须靠外层 timeout 兜底返回 false，而非永久挂起。
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // 接受连接后保持打开但不读不写（模拟卡死），持有连接直到 task 结束。
            // 测试返回后 tokio runtime 销毁会取消该 task，不会真等满 sleep。
            if let Ok((_sock, _)) = listener.accept().await {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });

        let start = std::time::Instant::now();
        let result = check_vnc_rfb_ready(port, 500).await;
        assert!(!result);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(450),
            "should wait ~timeout, got {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(1500),
            "should not hang, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn rfb_ready_returns_false_when_port_closed() {
        // 1 号端口几乎必然拒绝连接
        assert!(!check_vnc_rfb_ready(1, 500).await);
    }
}
