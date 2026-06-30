//! gRPC 服务通用探测工具（端口探测）
//!
//! VNC 专用探测（含 RFB 完整握手、probe 编排）见 [`super::vnc_probe`]。

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
