//! 容器管理服务 - 统一管理Docker容器的生命周期

use anyhow::Result;
use dashmap::DashMap;
use docker_manager::{DockerContainerInfo, DockerManager};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use super::{container_monitor::GLOBAL_CONTAINER_MONITOR, port_manager::GLOBAL_PORT_MANAGER};

/// 容器管理服务
pub struct ContainerService {
    /// Docker管理器
    docker_manager: Arc<DockerManager>,
    /// 项目到容器信息的映射
    project_containers: Arc<DashMap<String, ContainerServiceInfo>>,
}

/// 容器服务信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerServiceInfo {
    /// 项目ID
    pub project_id: String,
    /// 容器信息
    pub container_info: DockerContainerInfo,
    /// 分配的端口
    pub allocated_port: Option<u16>,
    /// 创建时间
    pub created_at: std::time::SystemTime,
    /// 服务状态
    pub status: ContainerServiceStatus,
}

/// 容器服务状态
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContainerServiceStatus {
    /// 创建中
    Creating,
    /// 运行中
    Running,
    /// 停止中
    Stopping,
    /// 已停止
    Stopped,
    /// 错误状态
    Error(String),
}

impl ContainerService {
    /// 创建新的容器管理服务
    pub async fn new() -> Result<Self> {
        let docker_manager = Arc::new(DockerManager::with_default_config().await?);

        // 初始化容器监控器
        if let Some(monitor) = super::container_monitor::get_global_container_monitor().await {
            info!("容器监控器已初始化");
        } else {
            // 如果监控器未初始化，则初始化
            super::container_monitor::init_global_container_monitor(docker_manager.clone()).await?;
            info!("容器监控器已初始化");
        }

        Ok(Self {
            docker_manager,
            project_containers: Arc::new(DashMap::new()),
        })
    }

    /// 为项目创建并启动容器
    pub async fn create_container_for_project(
        &self,
        project_id: &str,
        project_path: &std::path::Path,
        model_config: Option<shared_types::ModelProviderConfig>,
    ) -> Result<u16> {
        info!("为项目创建容器: {}", project_id);

        // 检查是否已存在容器的服务
        if let Some(service_info) = self.project_containers.get(project_id) {
            warn!("项目 {} 已有容器服务，将先停止", project_id);
            self.stop_container_for_project(project_id).await?;
        }

        // 分配端口
        let allocated_port = GLOBAL_PORT_MANAGER.allocate_port().await
            .map_err(|e| anyhow::anyhow!("端口分配失败: {}", e))?;

        // 创建容器配置
        let container_config = self.create_container_config(
            project_id,
            project_path,
            allocated_port,
            model_config.as_ref(),
        )?;

        // 更新服务状态
        let service_info = ContainerServiceInfo {
            project_id: project_id.to_string(),
            container_info: DockerContainerInfo {
                // 这里先创建一个临时的container_info，实际会在create_container中替换
                container_id: String::new(),
                container_name: String::new(),
                project_id: project_id.to_string(),
                image: container_config.image.clone(),
                status: docker_manager::ContainerStatus::Creating,
                created_at: chrono::Utc::now(),
                started_at: None,
                host_path: container_config.host_path.clone(),
                container_path: container_config.container_path.clone(),
                port_bindings: container_config.port_bindings.clone(),
                health_status: None,
            },
            allocated_port: Some(allocated_port),
            created_at: chrono::Utc::now().into(),
            status: ContainerServiceStatus::Creating,
        };

        self.project_containers.insert(project_id.to_string(), service_info);

        // 创建并启动容器
        let container_info = match self.docker_manager.create_container(container_config).await {
            Ok(info) => info,
            Err(e) => {
                // 创建失败，清理资源
                error!("创建容器失败: {}", e);
                self.project_containers.remove(project_id);
                GLOBAL_PORT_MANAGER.release_port(allocated_port).await;
                return Err(anyhow::anyhow!("创建容器失败: {}", e));
            }
        };

        // 更新服务信息
        if let Some(mut service_info) = self.project_containers.get_mut(project_id) {
            service_info.container_info = container_info.clone();
            service_info.status = ContainerServiceStatus::Running;
        }

        info!("容器创建成功: {} (端口: {})", container_info.container_name, allocated_port);

        // 添加到监控器
        if let Some(monitor) = super::container_monitor::get_global_container_monitor().await {
            monitor.add_container(&container_info, Some(allocated_port));
        }

        Ok(allocated_port)
    }

    /// 停止项目的容器
    pub async fn stop_container_for_project(&self, project_id: &str) -> Result<()> {
        info!("停止项目的容器: {}", project_id);

        let service_info = if let Some(info) = self.project_containers.get(project_id) {
            info.clone()
        } else {
            warn!("项目 {} 没有找到容器服务", project_id);
            return Ok(());
        };

        // 更新状态
        if let Some(mut info) = self.project_containers.get_mut(project_id) {
            info.status = ContainerServiceStatus::Stopping;
        }

        // 停止容器
        if let Err(e) = self.docker_manager.stop_container(project_id).await {
            error!("停止容器失败: {}", e);
            if let Some(mut info) = self.project_containers.get_mut(project_id) {
                info.status = ContainerServiceStatus::Error(e.to_string());
            }
            return Err(anyhow::anyhow!("停止容器失败: {}", e));
        }

        // 释放端口
        if let Some(port) = service_info.allocated_port {
            GLOBAL_PORT_MANAGER.release_port(port).await;
            info!("释放端口: {}", port);
        }

        // 从监控器移除
        if let Some(monitor) = super::container_monitor::get_global_container_monitor().await {
            monitor.remove_container(project_id);
        }

        // 更新状态
        if let Some(mut info) = self.project_containers.get_mut(project_id) {
            info.status = ContainerServiceStatus::Stopped;
        }

        info!("项目容器已停止: {}", project_id);
        Ok(())
    }

    /// 获取项目的容器信息
    pub fn get_container_info(&self, project_id: &str) -> Option<ContainerServiceInfo> {
        self.project_containers.get(project_id).map(|info| info.clone())
    }

    /// 获取所有容器服务信息
    pub fn get_all_containers(&self) -> Vec<ContainerServiceInfo> {
        self.project_containers
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// 清理所有容器（用于服务关闭时）
    pub async fn cleanup_all_containers(&self) -> Result<()> {
        info!("清理所有容器服务");

        let containers: Vec<String> = self.project_containers
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for project_id in containers {
            if let Err(e) = self.stop_container_for_project(&project_id).await {
                error!("清理容器失败 {}: {}", project_id, e);
            }
        }

        info!("所有容器服务已清理");
        Ok(())
    }

    /// 获取容器统计信息
    pub fn get_container_stats(&self) -> ContainerServiceStats {
        let containers = self.get_all_containers();
        let mut stats = ContainerServiceStats::default();

        for container in &containers {
            match container.status {
                ContainerServiceStatus::Creating => stats.creating_count += 1,
                ContainerServiceStatus::Running => stats.running_count += 1,
                ContainerServiceStatus::Stopping => stats.stopping_count += 1,
                ContainerServiceStatus::Stopped => stats.stopped_count += 1,
                ContainerServiceStatus::Error(_) => stats.error_count += 1,
            }
        }

        stats.total_count = containers.len();
        stats
    }

    /// 创建容器配置
    fn create_container_config(
        &self,
        project_id: &str,
        project_path: &std::path::Path,
        port: u16,
        model_config: Option<&shared_types::ModelProviderConfig>,
    ) -> Result<docker_manager::DockerContainerConfig> {
        let mut env_vars = std::collections::HashMap::new();

        // 设置基本环境变量
        env_vars.insert("RUST_LOG".to_string(), "info".to_string());
        env_vars.insert("PROJECT_ID".to_string(), project_id.to_string());
        env_vars.insert("AGENT_TYPE".to_string(), "claude".to_string());

        // 设置模型提供商环境变量
        if let Some(provider) = model_config {
            env_vars.insert("MODEL_PROVIDER_NAME".to_string(), provider.name.clone());
            env_vars.insert("MODEL_PROVIDER_API_KEY".to_string(), provider.api_key.clone());
            if !provider.base_url.is_empty() {
                env_vars.insert("MODEL_PROVIDER_BASE_URL".to_string(), provider.base_url.clone());
            }
            if !provider.default_model.is_empty() {
                env_vars.insert("MODEL_PROVIDER_DEFAULT_MODEL".to_string(), provider.default_model.clone());
            }
        }

        // 创建端口映射
        let mut port_bindings = std::collections::HashMap::new();
        port_bindings.insert("8086/tcp".to_string(), port.to_string());

        Ok(docker_manager::DockerContainerConfig {
            project_id: project_id.to_string(),
            image: "registry.yichamao.com/rcoder:latest".to_string(),
            name_prefix: "rcoder-agent".to_string(),
            host_path: project_path.to_string_lossy().to_string(),
            container_path: "/app/workspace".to_string(),
            work_dir: "/app/workspace".to_string(),
            env_vars,
            port_bindings,
            network_mode: "host".to_string(),
            auto_remove: true,
            resource_limits: Some(docker_manager::ResourceLimits {
                memory_limit: Some(2 * 1024 * 1024 * 1024), // 2GB 内存
                cpu_limit: Some(2.0), // 2 核 CPU
                swap_limit: Some(4 * 1024 * 1024 * 1024), // 4GB 交换空间
            }),
        })
    }
}

/// 容器服务统计信息
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContainerServiceStats {
    /// 总容器数
    pub total_count: usize,
    /// 创建中的容器数
    pub creating_count: usize,
    /// 运行中的容器数
    pub running_count: usize,
    /// 停止中的容器数
    pub stopping_count: usize,
    /// 已停止的容器数
    pub stopped_count: usize,
    /// 错误的容器数
    pub error_count: usize,
}

/// 全局容器服务实例
pub static GLOBAL_CONTAINER_SERVICE: std::sync::LazyLock<tokio::sync::RwLock<Option<Arc<ContainerService>>>> =
    std::sync::LazyLock::new(|| tokio::sync::RwLock::new(None));

/// 初始化全局容器服务
pub async fn init_global_container_service() -> Result<()> {
    let service = Arc::new(ContainerService::new().await?);
    let mut global_service = GLOBAL_CONTAINER_SERVICE.write().await;
    *global_service = Some(service);

    info!("全局容器服务已初始化");
    Ok(())
}

/// 获取全局容器服务
pub async fn get_global_container_service() -> Option<Arc<ContainerService>> {
    GLOBAL_CONTAINER_SERVICE.read().await.clone()
}