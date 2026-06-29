//! gRPC 服务通用探测工具（端口 / WebSocket）
//!
//! VNC 专用探测（含 RFB 握手、probe 编排）见 [`super::vnc_probe`]。

use std::time::Duration;

/// 检查 TCP 端口是否可达（仅 connect-accept，不验证应用层）
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
/// （端口可达 ≠ WebSocket 代理可用）
///
/// # 返回
/// - `true`: WebSocket 握手成功（返回 101 Switching Protocols）
/// - `false`: 握手失败或超时
pub async fn check_novnc_websocket_ready(port: u16, timeout_millis: u64) -> bool {
    use tokio_tungstenite::connect_async;

    let url = format!("ws://127.0.0.1:{}/websockify", port);

    match tokio::time::timeout(Duration::from_millis(timeout_millis), connect_async(&url)).await {
        Ok(Ok((mut ws_stream, response))) => {
            let status = response.status();
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
