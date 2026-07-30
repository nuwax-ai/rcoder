//! 网络管理：主网络检测、网络存在性检查（从 DockerManager 拆出）

use bollard::Docker;
use tracing::{debug, info, warn};

use crate::{DockerError, DockerManager, DockerResult};

impl DockerManager {
    /// 确保 RCoder 网络存在
    pub(crate) async fn ensure_rcoder_network(&self) -> DockerResult<()> {
        let main_network = self.get_main_network_name().await;
        info!("Checking RCoder network status: {}...", main_network);

        // 检查网络是否已存在
        match self.inspect_network(&main_network).await {
            Ok(_) => {
                info!("RCoder network already exists: {}", main_network);
                Ok(())
            }
            Err(_) => {
                warn!("RCoder network not found: {}", main_network);
                warn!("Network mode: bridge");
                warn!("Please check Docker Compose config");
                // 不创建网络，因为主网络应该由 Docker Compose 创建
                Ok(())
            }
        }
    }

    /// 检查网络是否存在
    async fn inspect_network(&self, network_name: &str) -> DockerResult<()> {
        use bollard::query_parameters::ListNetworksOptions;

        // 使用 list_networks 不带参数，然后手动过滤
        let networks = self
            .docker
            .list_networks(None::<ListNetworksOptions>)
            .await
            .map_err(|e| DockerError::ConnectionError(format!("failed to list networks: {}", e)))?;

        if networks
            .iter()
            .any(|n| n.name.as_ref() == Some(&network_name.to_string()))
        {
            Ok(())
        } else {
            Err(DockerError::ConnectionError(
                "Network does not exist".to_string(),
            ))
        }
    }

    pub(crate) async fn get_main_network_name(&self) -> String {
        self.main_network_name.read().await.clone()
    }

    /// 🔍 动态检测当前主容器所在的网络名称（静态方法，用于初始化）
    ///
    /// 通过检查当前容器（运行 DockerManager 的容器）所连接的网络来确定主网络名称
    /// 这样可以适应不同的 Docker Compose project name
    pub(crate) async fn detect_main_network_name_static(
        docker: &Docker,
        network_base_name: &str,
    ) -> DockerResult<String> {
        use bollard::query_parameters::InspectContainerOptions;

        // 🎯 优化：直接通过 HOSTNAME 环境变量 inspect 当前容器，无需列出所有容器
        let hostname = std::env::var("HOSTNAME").map_err(|_| {
            DockerError::ConnectionError(
                "unable to get HOSTNAME environment variable. Please ensure code is running in Docker container.".to_string(),
            )
        })?;

        debug!("Detecting container hostname: {}", hostname);

        // 直接 inspect 当前容器（hostname 通常是容器 ID 的前12位，但 Docker API 支持前缀匹配）
        let inspect = docker
            .inspect_container(&hostname, None::<InspectContainerOptions>)
            .await
            .map_err(|e| {
                DockerError::ConnectionError(format!(
                    "unable to get current container info (hostname: {}): {}",
                    hostname, e
                ))
            })?;

        // 获取网络配置
        if let Some(network_settings) = inspect.network_settings
            && let Some(networks) = network_settings.networks
        {
            // 查找包含指定网络基础名称的网络
            for network_name in networks.keys() {
                if network_name.contains(network_base_name) {
                    info!("detected network: {}", network_name);
                    return Ok(network_name.clone());
                }
            }

            // 如果没找到包含指定基础名称的，回退到第一个可用网络（Docker Compose 默认网络）
            if let Some(fallback_name) = networks.keys().next() {
                warn!(
                    "未找到包含 '{}' 的网络，回退使用当前容器的默认网络: {} (可用网络: {:?})",
                    network_base_name,
                    fallback_name,
                    networks.keys().collect::<Vec<_>>()
                );
                return Ok(fallback_name.clone());
            }

            return Err(DockerError::ConnectionError(
                "当前容器没有任何网络配置".to_string(),
            ));
        }

        Err(DockerError::ConnectionError(format!(
            "Current container (hostname: {}) has no network configuration information",
            hostname
        )))
    }

    /// 🔍 动态检测当前主容器所在的网络名称
    ///
    /// 通过检查当前容器（运行 DockerManager 的容器）所连接的网络来确定主网络名称
    /// 这样可以适应不同的 Docker Compose project name
    pub async fn detect_main_network_name(&self) -> DockerResult<String> {
        Self::detect_main_network_name_static(&self.docker, &self.config.network_base_name).await
    }
}
