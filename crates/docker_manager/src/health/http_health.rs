//! HTTP 健康检查
//!
//! 从 docker_container_agent.rs 迁移
//!
//! 健康检查通过 HTTP /health 端点验证服务是否就绪。
//! agent-runner 的 /health 端点会同时检查 HTTP 和 gRPC 服务，
//! 只有当两者都就绪时才返回 "healthy" 状态。

use crate::{DockerError, DockerResult};
use reqwest::Client;
use serde::Deserialize;
use shared_types::HttpResult;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, info, warn};

/// agent-runner 健康检查数据结构
#[derive(Deserialize)]
struct HealthData {
    /// 服务状态：healthy（完全就绪）、starting（启动中）
    status: String,
    /// HTTP 服务是否就绪
    http_ready: bool,
    /// gRPC 服务是否就绪
    grpc_ready: bool,
}

/// HTTP 健康检查器
pub struct HttpHealthChecker {
    client: Client,
    max_attempts: u32,
    timeout_seconds: u64,
}

impl HttpHealthChecker {
    /// 创建新的健康检查器
    ///
    /// # Arguments
    /// * `max_attempts` - 最大尝试次数
    /// * `timeout_seconds` - 每次尝试的超时时间(秒)
    pub fn new(max_attempts: u32, timeout_seconds: u64) -> Self {
        Self {
            client: Client::new(),
            max_attempts,
            timeout_seconds,
        }
    }

    /// 默认配置的健康检查器(60次，每次2秒，总计约180秒)
    /// 容器启动包含 MCP Proxy 等服务，可能需要 60-90 秒
    pub fn default_checker() -> Self {
        Self::new(60, 2)
    }

    /// 等待服务就绪
    ///
    /// 通过检查 /health 端点的响应判断服务是否就绪。
    /// agent-runner 的 /health 端点会同时检查 HTTP 和 gRPC 服务，
    /// 只有当两者都就绪时才返回 code="0000"。
    ///
    /// # Arguments
    /// * `base_url` - 服务基础URL (如: "http://172.17.0.2:8086")
    /// * `health_path` - 健康检查路径 (默认: "/health")
    ///
    /// # Returns
    /// * `DockerResult<()>` - 成功或超时错误
    pub async fn wait_for_ready(
        &self,
        base_url: &str,
        health_path: Option<&str>,
    ) -> DockerResult<()> {
        let health_url = format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            health_path.unwrap_or("health").trim_start_matches('/')
        );

        info!("🏥 [HEALTH] Health check started: {}", health_url);

        for attempt in 0..self.max_attempts {
            match timeout(
                Duration::from_secs(self.timeout_seconds),
                self.client.get(&health_url).send(),
            )
            .await
            {
                Ok(Ok(response)) if response.status().is_success() => {
                    // HTTP 状态码成功，进一步检查响应体
                    match response.json::<HttpResult<HealthData>>().await {
                        Ok(health_result) if health_result.code == "0000" => {
                            // 成功：code 为 "0000"
                            if let Some(health) = health_result.data {
                                info!(
                                    "✅ [HEALTH] Health check passed after {} attempts (status={}, http_ready={}, grpc_ready={})",
                                    attempt + 1,
                                    health.status,
                                    health.http_ready,
                                    health.grpc_ready
                                );
                            } else {
                                info!(
                                    "✅ [HEALTH] Health check passed after {} attempts",
                                    attempt + 1
                                );
                            }
                            return Ok(());
                        }
                        Ok(health_result) => {
                            // 服务未就绪：code 不是 "0000"
                            if let Some(health) = health_result.data {
                                warn!(
                                    "⏳ [HEALTH] Service not fully ready: code={}, message={}, status={}, http_ready={}, grpc_ready={}, waiting... ({}/{})",
                                    health_result.code,
                                    health_result.message,
                                    health.status,
                                    health.http_ready,
                                    health.grpc_ready,
                                    attempt + 1,
                                    self.max_attempts
                                );
                            } else {
                                warn!(
                                    "⏳ [HEALTH] Service not fully ready: code={}, message={}, waiting... ({}/{})",
                                    health_result.code,
                                    health_result.message,
                                    attempt + 1,
                                    self.max_attempts
                                );
                            }
                        }
                        Err(e) => {
                            debug!(
                                "❌ [HEALTH] Failed to parse health response: {}, waiting... ({}/{})",
                                e,
                                attempt + 1,
                                self.max_attempts
                            );
                        }
                    }
                }
                Ok(Ok(response)) => {
                    debug!(
                        "❌ [HEALTH] HTTP returned non-success status: {}, waiting... ({}/{})",
                        response.status(),
                        attempt + 1,
                        self.max_attempts
                    );
                }
                Ok(Err(e)) => {
                    debug!(
                        "❌ [HEALTH] Connection failed: {}, continuing to wait... ({}/{})",
                        e,
                        attempt + 1,
                        self.max_attempts
                    );
                }
                Err(_) => {
                    debug!(
                        "⏱️ [HEALTH] Connection timeout, continuing to wait... ({}/{})",
                        attempt + 1,
                        self.max_attempts
                    );
                }
            }

            // 每 10 次尝试输出一次 info 日志
            if (attempt + 1) % 10 == 0 {
                info!(
                    "⏳ [HEALTH] Still waiting for service to start... ({}/{})",
                    attempt + 1,
                    self.max_attempts
                );
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        Err(DockerError::ContainerStartError(format!(
            "Wait for service startup timeout: {} (attempted {} times)",
            health_url, self.max_attempts
        )))
    }
}

/// 便捷函数: 等待服务就绪(使用默认配置)
///
/// 检查 /health 端点，等待 agent-runner 的 HTTP 和 gRPC 服务都就绪。
///
/// # Arguments
/// * `base_url` - 服务基础URL
///
/// # Returns
/// * `DockerResult<()>` - 成功或超时错误
pub async fn wait_for_service_ready(base_url: &str) -> DockerResult<()> {
    HttpHealthChecker::default_checker()
        .wait_for_ready(base_url, None)
        .await
}
