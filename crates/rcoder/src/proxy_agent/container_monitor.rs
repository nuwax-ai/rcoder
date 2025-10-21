//! 容器监控器 - 监控Docker容器的健康状态和资源使用

use anyhow::Result;
use dashmap::DashMap;
use docker_manager::{DockerContainerInfo, DockerManager};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

/// 容器健康状态
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContainerHealthStatus {
    /// 健康
    Healthy,
    /// 不健康
    Unhealthy,
    /// 未知
    Unknown,
    /// 容器已停止
    Stopped,
}

/// 容器监控信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerMonitorInfo {
    /// 项目ID
    pub project_id: String,
    /// 容器ID
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 健康状态
    pub health_status: ContainerHealthStatus,
    /// 最后检查时间
    pub last_check: std::time::SystemTime,
    /// 连续失败次数
    pub consecutive_failures: u32,
    /// 容器启动时间
    pub started_at: Option<std::time::SystemTime>,
    /// 分配的端口
    pub allocated_port: Option<u16>,
}

/// 容器监控器
pub struct ContainerMonitor {
    /// Docker管理器
    docker_manager: Arc<DockerManager>,
    /// 监控的容器信息
    monitored_containers: Arc<DashMap<String, ContainerMonitorInfo>>,
    /// 健康检查间隔
    health_check_interval: Duration,
    /// 最大失败次数
    max_failures: u32,
}

impl ContainerMonitor {
    /// 创建新的容器监控器
    pub fn new(docker_manager: Arc<DockerManager>) -> Self {
        Self {
            docker_manager,
            monitored_containers: Arc::new(DashMap::new()),
            health_check_interval: Duration::from_secs(30),
            max_failures: 3,
        }
    }

    /// 添加容器到监控列表
    pub fn add_container(&self, container_info: &DockerContainerInfo, allocated_port: Option<u16>) {
        let monitor_info = ContainerMonitorInfo {
            project_id: container_info.project_id.clone(),
            container_id: container_info.container_id.clone(),
            container_name: container_info.container_name.clone(),
            health_status: ContainerHealthStatus::Unknown,
            last_check: std::time::SystemTime::now(),
            consecutive_failures: 0,
            started_at: container_info.started_at.map(|dt| {
                dt.naive_utc().and_utc().into()
            }),
            allocated_port,
        };

        self.monitored_containers.insert(
            container_info.project_id.clone(),
            monitor_info,
        );

        info!("添加容器到监控列表: {} ({})",
              container_info.container_name, container_info.project_id);
    }

    /// 从监控列表移除容器
    pub fn remove_container(&self, project_id: &str) {
        if let Some((_, info)) = self.monitored_containers.remove(project_id) {
            info!("从监控列表移除容器: {} ({})",
                  info.container_name, project_id);
        }
    }

    /// 获取容器监控信息
    pub fn get_container_info(&self, project_id: &str) -> Option<ContainerMonitorInfo> {
        self.monitored_containers.get(project_id).map(|info| info.clone())
    }

    /// 获取所有监控的容器
    pub fn get_all_containers(&self) -> Vec<ContainerMonitorInfo> {
        self.monitored_containers
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// 启动健康检查任务
    pub async fn start_health_check_task(&self) -> Result<()> {
        let monitor = self.clone();
        let mut interval = interval(self.health_check_interval);

        tokio::spawn(async move {
            loop {
                interval.tick().await;
                monitor.perform_health_checks().await;
            }
        });

        info!("容器健康检查任务已启动");
        Ok(())
    }

    /// 执行健康检查
    async fn perform_health_checks(&self) {
        debug!("执行容器健康检查，监控容器数量: {}",
               self.monitored_containers.len());

        let containers_to_check: Vec<String> = self.monitored_containers
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for project_id in containers_to_check {
            if let Some(container_info) = self.get_container_info(&project_id) {
                self.check_container_health(&container_info).await;
            }
        }
    }

    /// 检查单个容器的健康状态
    async fn check_container_health(&self, monitor_info: &ContainerMonitorInfo) {
        let health_url = if let Some(port) = monitor_info.allocated_port {
            format!("http://localhost:{}/health", port)
        } else {
            warn!("容器 {} 没有分配端口，跳过健康检查", monitor_info.container_name);
            return;
        };

        debug!("检查容器健康状态: {} ({})",
               monitor_info.container_name, health_url);

        let client = reqwest::Client::new();
        let is_healthy = match client.get(&health_url).send().await {
            Ok(response) => response.status().is_success(),
            Err(e) => {
                debug!("健康检查请求失败: {} - {}", monitor_info.container_name, e);
                false
            }
        };

        self.update_container_health_status(&monitor_info.project_id, is_healthy).await;
    }

    /// 更新容器健康状态
    async fn update_container_health_status(&self, project_id: &str, is_healthy: bool) {
        if let Some(mut monitor_info) = self.monitored_containers.get_mut(project_id) {
            let old_status = monitor_info.health_status.clone();
            monitor_info.last_check = std::time::SystemTime::now();

            if is_healthy {
                monitor_info.health_status = ContainerHealthStatus::Healthy;
                monitor_info.consecutive_failures = 0;

                if old_status != ContainerHealthStatus::Healthy {
                    info!("容器健康状态恢复正常: {} ({})",
                          monitor_info.container_name, project_id);
                }
            } else {
                monitor_info.consecutive_failures += 1;

                if monitor_info.consecutive_failures >= self.max_failures {
                    monitor_info.health_status = ContainerHealthStatus::Unhealthy;
                    warn!("容器健康状态变为不健康: {} ({}) (失败次数: {})",
                         monitor_info.container_name, project_id, monitor_info.consecutive_failures);

                    // 触发容器重启或清理逻辑
                    self.handle_unhealthy_container(project_id).await;
                }
            }
        }
    }

    /// 处理不健康的容器
    async fn handle_unhealthy_container(&self, project_id: &str) {
        error!("处理不健康的容器: {}", project_id);

        // 这里可以实现自动重启逻辑
        // 1. 停止当前容器
        // 2. 释放端口资源
        // 3. 重新创建容器

        if let Some(monitor_info) = self.get_container_info(project_id) {
            // 停止容器
            if let Err(e) = self.docker_manager.stop_container(project_id).await {
                error!("停止不健康容器失败: {}", e);
            }

            // 释放端口
            if let Some(port) = monitor_info.allocated_port {
                crate::proxy_agent::port_manager::GLOBAL_PORT_MANAGER.release_port(port).await;
                info!("释放不健康容器的端口: {}", port);
            }

            // 从监控列表移除
            self.remove_container(project_id);
        }
    }

    /// 获取健康统计信息
    pub fn get_health_stats(&self) -> ContainerHealthStats {
        let containers = self.get_all_containers();
        let mut stats = ContainerHealthStats::default();

        for container in &containers {
            match container.health_status {
                ContainerHealthStatus::Healthy => stats.healthy_count += 1,
                ContainerHealthStatus::Unhealthy => stats.unhealthy_count += 1,
                ContainerHealthStatus::Unknown => stats.unknown_count += 1,
                ContainerHealthStatus::Stopped => stats.stopped_count += 1,
            }
        }

        stats.total_count = containers.len();
        stats
    }
}

impl Clone for ContainerMonitor {
    fn clone(&self) -> Self {
        Self {
            docker_manager: self.docker_manager.clone(),
            monitored_containers: self.monitored_containers.clone(),
            health_check_interval: self.health_check_interval,
            max_failures: self.max_failures,
        }
    }
}

/// 容器健康统计信息
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContainerHealthStats {
    /// 总容器数
    pub total_count: usize,
    /// 健康容器数
    pub healthy_count: usize,
    /// 不健康容器数
    pub unhealthy_count: usize,
    /// 未知状态容器数
    pub unknown_count: usize,
    /// 已停止容器数
    pub stopped_count: usize,
}

/// 全局容器监控器实例
pub static GLOBAL_CONTAINER_MONITOR: std::sync::LazyLock<tokio::sync::RwLock<Option<Arc<ContainerMonitor>>>> =
    std::sync::LazyLock::new(|| tokio::sync::RwLock::new(None));

/// 初始化全局容器监控器
pub async fn init_global_container_monitor(docker_manager: Arc<DockerManager>) -> Result<()> {
    let monitor = Arc::new(ContainerMonitor::new(docker_manager));
    monitor.start_health_check_task().await?;

    let mut global_monitor = GLOBAL_CONTAINER_MONITOR.write().await;
    *global_monitor = Some(monitor);

    info!("全局容器监控器已初始化");
    Ok(())
}

/// 获取全局容器监控器
pub async fn get_global_container_monitor() -> Option<Arc<ContainerMonitor>> {
    GLOBAL_CONTAINER_MONITOR.read().await.clone()
}