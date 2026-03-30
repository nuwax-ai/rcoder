//! HTTP 健康检查
//!
//! 从 docker_container_agent.rs 迁移

use crate::{DockerError, DockerResult};
use reqwest::Client;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, info};

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

 info!(" message started: {}", health_url);

        for attempt in 0..self.max_attempts {
            match timeout(
                Duration::from_secs(self.timeout_seconds),
                self.client.get(&health_url).send(),
            )
            .await
            {
                Ok(Ok(response)) if response.status().is_success() => {
 info!(" message already message ");
                    return Ok(());
                }
                Ok(Ok(response)) => {
                    debug!(
                        "服务返回非成功状态: {}, 等待中... ({}/{})",
                        response.status(),
                        attempt + 1,
                        self.max_attempts
                    );
                }
                Ok(Err(e)) => {
                    debug!(
                        "连接失败: {}, 继续等待... ({}/{})",
                        e,
                        attempt + 1,
                        self.max_attempts
                    );
                }
                Err(_) => {
                    debug!(
                        "连接超时, 继续等待... ({}/{})",
                        attempt + 1,
                        self.max_attempts
                    );
                }
            }

            // 每 10 次尝试输出一次 info 日志
            if (attempt + 1) % 10 == 0 {
                info!(
                    "仍在等待服务启动... ({}/{})",
                    attempt + 1,
                    self.max_attempts
                );
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        Err(DockerError::ContainerStartError(format!(
            "等待服务启动超时: {} (尝试{}次)",
            health_url, self.max_attempts
        )))
    }
}

/// 便捷函数: 等待服务就绪(使用默认配置)
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
