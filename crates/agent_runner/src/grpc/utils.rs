//! gRPC 服务工具函数

use std::time::Duration;

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
