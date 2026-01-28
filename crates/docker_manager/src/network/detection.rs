//! 网络检测功能
//!
//! 从 container_manager.rs 迁移的网络检测逻辑

use crate::{DockerError, DockerResult, RCODER_NETWORK_BASE_NAME};
use bollard::{query_parameters::InspectContainerOptions, Docker};
use tracing::{debug, info, warn};

/// 网络检测器
pub struct NetworkDetector<'a> {
    docker: &'a Docker,
}

impl<'a> NetworkDetector<'a> {
    /// 创建新的网络检测器
    pub fn new(docker: &'a Docker) -> Self {
        Self { docker }
    }

    /// 动态检测当前主容器所在的网络名称
    ///
    /// 通过检查当前容器的网络配置来确定主网络名称
    ///
    /// # Returns
    /// * `DockerResult<String>` - 网络名称或错误
    pub async fn detect_main_network(&self) -> DockerResult<String> {
        // 从 HOSTNAME 环境变量获取容器ID
        let hostname = std::env::var("HOSTNAME").map_err(|_| {
            DockerError::ConnectionError(
                "无法获取 HOSTNAME 环境变量。请确保代码运行在 Docker 容器中。".to_string(),
            )
        })?;

        debug!("检测到容器 hostname: {}", hostname);

        // Inspect 当前容器
        let inspect = self
            .docker
            .inspect_container(&hostname, None::<InspectContainerOptions>)
            .await
            .map_err(|e| {
                DockerError::ConnectionError(format!(
                    "无法获取当前容器信息 (hostname: {}): {}",
                    hostname, e
                ))
            })?;

        // 获取网络配置
        if let Some(network_settings) = inspect.network_settings
            && let Some(networks) = network_settings.networks {
                // 查找包含 "agent-network" 的网络
                for network_name in networks.keys() {
                    if network_name.contains(RCODER_NETWORK_BASE_NAME) {
                        info!("✅ 动态检测到主网络: {}", network_name);
                        return Ok(network_name.clone());
                    }
                }

                // 如果没找到,返回错误
                let available_networks: Vec<String> = networks.keys().cloned().collect();
                return Err(DockerError::ConnectionError(format!(
                    "当前容器未连接到包含 '{}' 的网络。\n\
                     可用网络: {:?}\n\
                     请检查 Docker Compose 配置中的网络设置。",
                    RCODER_NETWORK_BASE_NAME, available_networks
                )));
            }

        Err(DockerError::ConnectionError(format!(
            "当前容器 (hostname: {}) 没有网络配置信息",
            hostname
        )))
    }

    /// 从容器 labels 获取 Compose 项目名称
    ///
    /// # Arguments
    /// * `container_id` - 容器ID或hostname
    ///
    /// # Returns
    /// * `DockerResult<Option<String>>` - 项目名称或None
    pub async fn get_compose_project_from_labels(
        &self,
        container_id: &str,
    ) -> DockerResult<Option<String>> {
        let inspect = self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| DockerError::ConnectionError(format!("获取容器信息失败: {}", e)))?;

        // 从 labels 中获取项目名称
        if let Some(labels) = inspect.config.and_then(|c| c.labels) {
            // Docker Compose 会添加 com.docker.compose.project 标签
            if let Some(project_name) = labels.get("com.docker.compose.project") {
                info!("通过容器 labels 获取项目名称: {}", project_name);
                return Ok(Some(project_name.clone()));
            }
        }

        Ok(None)
    }

    /// 从容器名称推断 Compose 项目名称
    ///
    /// # Arguments
    /// * `container_id` - 容器ID或hostname
    ///
    /// # Returns
    /// * `DockerResult<Option<String>>` - 项目名称或None
    pub async fn get_compose_project_from_name(
        &self,
        container_id: &str,
    ) -> DockerResult<Option<String>> {
        let inspect = self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| DockerError::ConnectionError(format!("获取容器信息失败: {}", e)))?;

        // 从容器名称推断项目名称
        if let Some(name) = inspect.name {
            // 容器名称格式: /{project_name}-{service_name}-{number}
            let clean_name = name.trim_start_matches('/');
            if let Some(project_name) = clean_name.split('-').next() {
                info!("通过容器名称推断项目名称: {}", project_name);
                return Ok(Some(project_name.to_string()));
            }
        }

        Ok(None)
    }

    /// 动态获取 Compose 项目名称(多种策略)
    ///
    /// 尝试多种方法获取项目名称:
    /// 1. 环境变量 COMPOSE_PROJECT_NAME
    /// 2. 容器 labels
    /// 3. 容器名称推断
    ///
    /// # Arguments
    /// * `container_id` - 容器ID或hostname (None表示使用当前容器)
    ///
    /// # Returns
    /// * `DockerResult<Option<String>>` - 项目名称或None
    pub async fn get_dynamic_compose_project(
        &self,
        container_id: Option<&str>,
    ) -> DockerResult<Option<String>> {
        // 方法1: 环境变量
        if let Ok(project_name) = std::env::var("COMPOSE_PROJECT_NAME") {
            info!("通过环境变量获取项目名称: {}", project_name);
            return Ok(Some(project_name));
        }

        let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "self".to_string());
        let cid = container_id.unwrap_or(&hostname);

        // 方法2: 容器 labels
        if let Some(project_name) = self.get_compose_project_from_labels(cid).await? {
            return Ok(Some(project_name));
        }

        // 方法3: 容器名称推断
        if let Some(project_name) = self.get_compose_project_from_name(cid).await? {
            return Ok(Some(project_name));
        }

        warn!("无法获取 Docker Compose 项目名称");
        Ok(None)
    }
}
