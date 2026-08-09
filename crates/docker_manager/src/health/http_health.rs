//! HTTP 健康检查
//!
//! 从 docker_container_agent.rs 迁移
//!
//! 健康检查通过 HTTP /health 端点验证服务是否就绪。
//! agent-runner 的 /health 端点会同时检查 HTTP 和 gRPC 服务，
//! 只有当两者都就绪时才返回 "healthy" 状态。

use crate::{DockerError, DockerResult};
use reqwest::Client;
use shared_types::HttpResult;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tracing::{debug, info, warn};

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

        info!(
            "🏥 [HEALTH] Health check started: {} (max_attempts={}, timeout_per_attempt={}s)",
            health_url, self.max_attempts, self.timeout_seconds
        );

        let health_started = Instant::now();

        for attempt in 0..self.max_attempts {
            match timeout(
                Duration::from_secs(self.timeout_seconds),
                self.client.get(&health_url).send(),
            )
            .await
            {
                Ok(Ok(response)) if response.status().is_success() => {
                    // HTTP 状态码成功，进一步检查响应体
                    match response
                        .json::<HttpResult<shared_types::HealthCheckResponse>>()
                        .await
                    {
                        Ok(health_result) if health_result.code == "0000" => {
                            // 成功：code 为 "0000"
                            if let Some(health) = health_result.data {
                                info!(
                                    "✅ [HEALTH] Health check passed after {} attempts in {:?} (status={}, http_ready={}, grpc_ready={})",
                                    attempt + 1,
                                    health_started.elapsed(),
                                    health.status,
                                    health.http_ready,
                                    health.grpc_ready
                                );
                            } else {
                                info!(
                                    "✅ [HEALTH] Health check passed after {} attempts in {:?}",
                                    attempt + 1,
                                    health_started.elapsed()
                                );
                            }
                            return Ok(());
                        }
                        Ok(health_result) => {
                            // 服务未就绪：code 不是 "0000"
                            if let Some(health) = health_result.data {
                                warn!(
                                    "⏳ [HEALTH] Service not fully ready: code={}, message={}, status={}, http_ready={}, grpc_ready={}, elapsed={:?}, waiting... ({}/{})",
                                    health_result.code,
                                    health_result.message,
                                    health.status,
                                    health.http_ready,
                                    health.grpc_ready,
                                    health_started.elapsed(),
                                    attempt + 1,
                                    self.max_attempts
                                );
                            } else {
                                warn!(
                                    "⏳ [HEALTH] Service not fully ready: code={}, message={}, elapsed={:?}, waiting... ({}/{})",
                                    health_result.code,
                                    health_result.message,
                                    health_started.elapsed(),
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

            // 每 5 次尝试输出一次 info 进度日志
            if (attempt + 1) % 5 == 0 {
                info!(
                    "⏳ [HEALTH] Still waiting for service to start... ({}/{}), elapsed={:?}",
                    attempt + 1,
                    self.max_attempts,
                    health_started.elapsed()
                );
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        Err(DockerError::ContainerStartError(format!(
            "Wait for service startup timeout: {} (attempted {} times, elapsed {:?})",
            health_url,
            self.max_attempts,
            health_started.elapsed()
        )))
    }
}

/// 便捷函数: 等待服务就绪(使用默认配置)
///
/// 检查 /ready 端点，等待 agent-runner 的 gRPC 服务就绪（readiness）。
///
/// 用 /ready（而非 /health）: /health 现为 liveness（纯进程活恒 200, 不查 gRPC）,
/// /ready 才反映 gRPC 就绪（gRPC 没起返 503, 起了 200）。rcoder 创建 agent-runner 后
/// 需等 gRPC 就绪才能转发请求, 故探 /ready。
///
/// # Arguments
/// * `base_url` - 服务基础URL
///
/// # Returns
/// * `DockerResult<()>` - 成功或超时错误
pub async fn wait_for_service_ready(base_url: &str) -> DockerResult<()> {
    HttpHealthChecker::default_checker()
        .wait_for_ready(base_url, Some("ready"))
        .await
}
