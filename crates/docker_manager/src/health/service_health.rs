//! 服务健康检查模块
//!
//! 提供服务层面的健康检查功能，包括 HTTP 健康端点和 gRPC 连接检查。
//! 这是对 Docker 容器状态检查的补充，用于确认容器内服务是否真正可用。

use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, warn};

/// 默认 HTTP 健康检查端口
pub const DEFAULT_HTTP_HEALTH_PORT: u16 = 8086;

/// 默认 gRPC 服务端口
pub const DEFAULT_GRPC_PORT: u16 = 50051;

/// 服务健康检查超时时间（秒）
const HEALTH_CHECK_TIMEOUT_SECS: u64 = 3;

/// 服务健康状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceHealthStatus {
    /// HTTP 健康端点是否可达
    pub http_healthy: bool,
    /// gRPC 端口是否可连接
    pub grpc_healthy: bool,
    /// 上次检查时间
    pub last_check_time: DateTime<Utc>,
    /// 连续失败次数（HTTP 和 gRPC 都失败算一次）
    /// TODO: 用于后续自动重启功能
    pub consecutive_failures: u32,
}

impl ServiceHealthStatus {
    /// 创建新的健康状态（初始化为未知状态）
    pub fn new() -> Self {
        Self {
            http_healthy: false,
            grpc_healthy: false,
            last_check_time: Utc::now(),
            consecutive_failures: 0,
        }
    }

    /// 检查服务是否完全健康（HTTP 和 gRPC 都正常）
    pub fn is_fully_healthy(&self) -> bool {
        self.http_healthy && self.grpc_healthy
    }

    /// 检查服务是否部分健康（至少有一个正常）
    pub fn is_partially_healthy(&self) -> bool {
        self.http_healthy || self.grpc_healthy
    }
}

impl Default for ServiceHealthStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// 服务健康检查器
pub struct ServiceHealthChecker {
    client: Client,
    http_port: u16,
    grpc_port: u16,
    timeout_secs: u64,
}

impl ServiceHealthChecker {
    /// 创建新的健康检查器
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(HEALTH_CHECK_TIMEOUT_SECS))
                .build()
                .unwrap_or_else(|_| Client::new()),
            http_port: DEFAULT_HTTP_HEALTH_PORT,
            grpc_port: DEFAULT_GRPC_PORT,
            timeout_secs: HEALTH_CHECK_TIMEOUT_SECS,
        }
    }

    /// 使用自定义端口创建检查器
    pub fn with_ports(http_port: u16, grpc_port: u16) -> Self {
        Self {
            http_port,
            grpc_port,
            ..Self::new()
        }
    }

    /// 检查 HTTP 健康端点
    ///
    /// # Arguments
    /// * `ip` - 容器 IP 地址
    ///
    /// # Returns
    /// * `true` - 健康端点返回成功状态码
    /// * `false` - 连接失败或返回错误状态码
    pub async fn check_http_health(&self, ip: &str) -> bool {
        let url = format!("http://{}:{}/health", ip, self.http_port);

        match timeout(
            Duration::from_secs(self.timeout_secs),
            self.client.get(&url).send(),
        )
        .await
        {
            Ok(Ok(response)) => {
                let is_healthy = response.status().is_success();
                debug!(
                    "HTTP health check {}: status={}, healthy={}",
                    url,
                    response.status(),
                    is_healthy
                );
                is_healthy
            }
            Ok(Err(e)) => {
                debug!("HTTP health check failed {}: {}", url, e);
                false
            }
            Err(_) => {
                debug!("HTTP health check timeout {}", url);
                false
            }
        }
    }

    /// 检查 gRPC 端口连通性
    ///
    /// 通过 TCP 连接测试 gRPC 端口是否可达。
    /// 注意：这只是连接测试，不执行实际的 gRPC 健康检查协议。
    ///
    /// # Arguments
    /// * `ip` - 容器 IP 地址
    ///
    /// # Returns
    /// * `true` - 能够建立 TCP 连接
    /// * `false` - 连接失败或超时
    pub async fn check_grpc_connectivity(&self, ip: &str) -> bool {
        let addr = format!("{}:{}", ip, self.grpc_port);

        match timeout(
            Duration::from_secs(self.timeout_secs),
            TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Ok(_stream)) => {
                debug!("gRPC port connection: {}", addr);
                true
            }
            Ok(Err(e)) => {
                debug!("gRPC portconnectionfailed {}: {}", addr, e);
                false
            }
            Err(_) => {
                debug!("gRPC portconnectiontimeout {}", addr);
                false
            }
        }
    }

    /// 执行完整的服务健康检查
    ///
    /// 同时检查 HTTP 健康端点和 gRPC 端口连通性。
    ///
    /// # Arguments
    /// * `container_ip` - 容器 IP 地址
    /// * `previous_failures` - 之前的连续失败次数（用于累加）
    ///
    /// # Returns
    /// * `ServiceHealthStatus` - 包含所有检查结果的健康状态
    pub async fn check_service(
        &self,
        container_ip: &str,
        previous_failures: u32,
    ) -> ServiceHealthStatus {
        // 并行执行两个检查
        let (http_healthy, grpc_healthy) = tokio::join!(
            self.check_http_health(container_ip),
            self.check_grpc_connectivity(container_ip)
        );

        let is_fully_healthy = http_healthy && grpc_healthy;

        // 更新连续失败次数
        let consecutive_failures = if is_fully_healthy {
            0 // 完全健康，重置计数
        } else {
            previous_failures + 1
        };

        if !is_fully_healthy {
            warn!(
                "Service health check: IP={}, HTTP={}, gRPC={}, consecutive_failures={}",
                container_ip, http_healthy, grpc_healthy, consecutive_failures
            );
        }

        ServiceHealthStatus {
            http_healthy,
            grpc_healthy,
            last_check_time: Utc::now(),
            consecutive_failures,
        }
    }
}

impl Default for ServiceHealthChecker {
    fn default() -> Self {
        Self::new()
    }
}

/// 便捷函数：使用默认配置执行服务健康检查
pub async fn check_service_health(container_ip: &str) -> ServiceHealthStatus {
    ServiceHealthChecker::new()
        .check_service(container_ip, 0)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_health_status_new() {
        let status = ServiceHealthStatus::new();
        assert!(!status.http_healthy);
        assert!(!status.grpc_healthy);
        assert_eq!(status.consecutive_failures, 0);
    }

    #[test]
    fn test_is_fully_healthy() {
        let mut status = ServiceHealthStatus::new();
        assert!(!status.is_fully_healthy());

        status.http_healthy = true;
        assert!(!status.is_fully_healthy());

        status.grpc_healthy = true;
        assert!(status.is_fully_healthy());
    }

    #[test]
    fn test_is_partially_healthy() {
        let mut status = ServiceHealthStatus::new();
        assert!(!status.is_partially_healthy());

        status.http_healthy = true;
        assert!(status.is_partially_healthy());

        status.http_healthy = false;
        status.grpc_healthy = true;
        assert!(status.is_partially_healthy());
    }
}
