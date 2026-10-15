//! gRPC 端口就绪探测
//!
//! 容器 "Running" ≠ 服务可用：agent 容器内 start-up.sh 串行初始化期间
//! gRPC 端口（50051）无人监听（冷启动窗口实测 3~15s，见 chat_forward
//! 重试注释）。dial 失败后用本函数等端口就绪再重试，替代固定 sleep——
//! 拨号失败率与盲等超时同时消除。

use std::time::Duration;

/// TCP 探测 gRPC 端口就绪（200ms 间隔轮询）
///
/// 探测到监听立即返回 true；超过 `timeout` 返回 false。
/// TCP 通 ≠ gRPC server 完全可服务（bind 后 accept 前有微窗口），
/// 调用方的重试循环兜底该残余窗口。
pub async fn wait_grpc_port_ready(addr: &str, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn port_ready_returns_true_when_listening() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        assert!(wait_grpc_port_ready(&addr, Duration::from_secs(2)).await);
    }

    #[tokio::test]
    async fn port_ready_returns_false_on_timeout() {
        // 绑一个端口后立刻关闭——大概率无人监听（极小概率被其他进程复用）
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        assert!(!wait_grpc_port_ready(&addr, Duration::from_millis(300)).await);
    }
}
