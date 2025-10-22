//! 增强容器管理服务 - 统一管理Docker容器的生命周期
//!
//! 增强功能：
//! - 容器空闲检测和自动清理
//! - 容器健康状态监控
//! - 容器使用统计和分析
//! - 自动化资源管理

use anyhow::Result;
use dashmap::DashMap;
use docker_manager::{DockerContainerInfo, DockerManager};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use chrono::{DateTime, Utc};

use super::{container_monitor::GLOBAL_CONTAINER_MONITOR, port_manager::GLOBAL_PORT_MANAGER};

/// 容器自动清理配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerCleanupConfig {
    /// 空闲超时时间（默认30分钟）
    #[serde(default = "1800")]
    pub idle_timeout_seconds: u64,
    /// 清理检查间隔（默认5分钟）
    #[serde(default = "300")]
    pub cleanup_check_interval_seconds: u64,
    /// 最大容器存活时间（默认24小时）
    #[serde(default = "86400")]
    pub max_container_lifetime_seconds: u64,
    /// 是否启用自动清理
    #[serde(default = "true")]
    pub enable_auto_cleanup: bool,
}

impl Default for ContainerCleanupConfig {
    fn default() -> Self {
        Self {
            idle_timeout_seconds: 1800, // 30分钟
            cleanup_check_interval_seconds: 300, // 5分钟
            max_container_lifetime_seconds: 86400, // 24小时
            enable_auto_cleanup: true,
        }
    }
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
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
    /// 请求计数
    pub request_count: u64,
    /// 是否正在使用中
    pub is_active: bool,
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

/// 增强容器管理服务
pub struct ContainerService {
    /// Docker管理器
    pub docker_manager: Arc<DockerManager>,
    /// 项目到容器信息的映射
    pub project_containers: Arc<DashMap<String, ContainerServiceInfo>>,
    /// 清理配置
    pub cleanup_config: ContainerCleanupConfig,
    /// 清理任务句柄
    pub cleanup_task_handle: Arc<tokio::sync::RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl ContainerService {
    /// 创建新的容器管理服务
    pub async fn new() -> Result<Self> {
        let cleanup_config = ContainerCleanupConfig::default();
        Self::new_with_config(cleanup_config).await
    }

    /// 使用指定配置创建容器管理服务
    pub async fn new_with_config(cleanup_config: ContainerCleanupConfig) -> Result<Self> {
        let docker_manager = Arc::new(DockerManager::with_default_config().await?);

        // 初始化容器监控器
        if let Some(monitor) = super::container_monitor::get_global_container_monitor().await {
            info!("容器监控器已初始化");
        } else {
            // 如果监控器未初始化，则初始化
            super::container_monitor::init_global_container_monitor(docker_manager.clone()).await?;
            info!("容器监控器已初始化");
        }

        let cleanup_task_handle = Arc::new(tokio::sync::RwLock::new(None));

        Ok(Self {
            docker_manager,
            project_containers: Arc::new(DashMap::new()),
            cleanup_config,
            cleanup_task_handle,
        })
    }

    /// 启动自动清理任务
    pub async fn start_auto_cleanup_task(service: Arc<Self>) {
        info!("🧹 启动容器自动清理任务");

        let cleanup_task = {
            let service_clone = service.clone();
            tokio::spawn(async move {
                let mut cleanup_interval = tokio::time::interval(Duration::from_secs(
                    service_clone.cleanup_config.cleanup_check_interval_seconds
                ));

                loop {
                    cleanup_interval.tick().await;
                    Self::perform_cleanup_check(&service_clone).await;
                }
            })
        };

        let mut handle = service.cleanup_task_handle.write().await;
        *handle = Some(cleanup_task);

        info!("✅ 容器自动清理任务已启动");
    }

    /// 执行清理检查
    async fn perform_cleanup_check(service: &Arc<Self>) {
        info!("🔍 执行容器清理检查");

        let containers_to_check: Vec<String> = service.project_containers
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for project_id in containers_to_check {
            Self::check_and_cleanup_container(service, &project_id).await;
        }

        info!("✅ 容器清理检查完成");
    }

    /// 检查并清理单个容器
    async fn check_and_cleanup_container(service: &Arc<Self>, project_id: &str) {
        if let Some(container_info) = service.project_containers.get(project_id) {
            // 检查容器是否在运行状态
            if container_info.status != ContainerServiceStatus::Running {
                return;
            }

            // 检查是否启用自动清理
            if !service.cleanup_config.enable_auto_cleanup {
                return;
            }

            // 检查是否超过最大存活时间
            let now = Utc::now();
            let container_created_utc = container_info.created_at.with_timezone(&chrono::FixedOffset::east_opt(0, 0)).unwrap_or_else(|| container_info.created_at);

            if now.signed_duration_since(container_created_utc).num_seconds() > service.cleanup_config.max_container_lifetime_seconds as i64 {
                info!("⏰ 容器超过最大存活时间，将清理: project_id={}, age_seconds={}",
                       project_id, now.signed_duration_since(container_created_utc).num_seconds());
                Self::cleanup_container(service, project_id).await;
                return;
            }

            // 检查是否空闲超时
            let idle_time = now.signed_duration_since(container_info.last_activity);
            if idle_time.num_seconds() > service.cleanup_config.idle_timeout_seconds as i64 {
                info!("⏱️ 容器空闲超时，将清理: project_id={}, idle_seconds={}",
                       project_id, idle_time.num_seconds());
                Self::cleanup_container(service, project_id).await;
                return;
            }
        }
    }

    /// 清理指定容器
    async fn cleanup_container(service: &Arc<Self>, project_id: &str) {
        info!("🗑️ 清理闲置容器: project_id={}", project_id);

        if let Err(e) = service.stop_container_for_project(project_id).await {
            error!("❌ 清理容器失败: project_id={}, error={}", project_id, e);
        } else {
            info!("✅ 闲置容器清理成功: project_id={}", project_id);
        }
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
                assigned_port: allocated_port,
                health_status: None,
            },
            allocated_port: Some(allocated_port),
            created_at: chrono::Utc::now().into(),
            status: ContainerServiceStatus::Creating,
            last_activity: Utc::now(),
            request_count: 0,
            is_active: true,
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

        // 添加到监控器
        if let Some(monitor) = super::container_monitor::get_global_container_monitor().await {
            monitor.add_container(&container_info, Some(allocated_port));
        }

        info!("容器创建成功: {} (端口: {})", container_info.container_name, allocated_port);
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
            if let Err(e) = self.stop_container_for_project(project_id).await {
                error!("清理容器失败 {}: {}", project_id, e);
            }
        }

        info!("所有容器服务已清理");
        Ok(())
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
            extra_mounts: Vec::new(),
            command: None,
        })
    }

    /// 更新容器活动时间
    pub async fn update_container_activity(&self, project_id: &str) {
        if let Some(mut service_info) = self.project_containers.get_mut(project_id) {
            service_info.last_activity = Utc::now();
            service_info.request_count += 1;
            service_info.is_active = true;
            info!("更新容器活动时间: project_id={}", project_id);
        }
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
}