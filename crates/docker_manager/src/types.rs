use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// Docker 容器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerContainerConfig {
    /// 项目 ID
    pub project_id: String,
    /// Docker 镜像
    pub image: String,
    /// 容器名称前缀
    pub name_prefix: String,
    /// 主机路径映射
    pub host_path: String,
    /// 容器内路径
    pub container_path: String,
    /// 工作目录
    pub work_dir: String,
    /// 环境变量
    pub env_vars: HashMap<String, String>,
    /// 端口映射
    pub port_bindings: HashMap<String, String>,
    /// 网络模式
    pub network_mode: String,
    /// 自动删除
    pub auto_remove: bool,
    /// 资源限制
    pub resource_limits: Option<ResourceLimits>,
    /// 额外的挂载点
    pub extra_mounts: Vec<MountPoint>,
    /// 启动命令
    pub command: Option<Vec<String>>,
}

/// 挂载点配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountPoint {
    /// 主机路径
    pub host_path: String,
    /// 容器内路径
    pub container_path: String,
    /// 是否只读
    pub read_only: bool,
}

/// 资源限制配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// 内存限制 (字节)
    pub memory_limit: Option<i64>,
    /// CPU 限制
    pub cpu_limit: Option<f64>,
    /// 交换空间限制 (字节)
    pub swap_limit: Option<i64>,
}

impl Default for DockerContainerConfig {
    fn default() -> Self {
        Self {
            project_id: String::new(),
            image: crate::DEFAULT_DOCKER_IMAGE.to_string(),
            name_prefix: "rcoder-agent".to_string(),
            host_path: String::new(),
            container_path: crate::DEFAULT_WORK_DIR.to_string(),
            work_dir: crate::DEFAULT_WORK_DIR.to_string(),
            env_vars: HashMap::new(),
            port_bindings: HashMap::new(),
            network_mode: crate::DEFAULT_NETWORK_MODE.to_string(),
            auto_remove: false,
            resource_limits: None,
            extra_mounts: Vec::new(),
            command: None,
        }
    }
}

/// Docker 容器信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerContainerInfo {
    /// 容器 ID
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 项目 ID
    pub project_id: String,
    /// 镜像名称
    pub image: String,
    /// 状态
    pub status: ContainerStatus,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 启动时间
    pub started_at: Option<DateTime<Utc>>,
    /// 主机路径
    pub host_path: String,
    /// 容器内路径
    pub container_path: String,
    /// 端口映射
    pub port_bindings: HashMap<String, String>,
    /// 分配的端口号
    pub assigned_port: u16,
    /// 健康检查状态
    pub health_status: Option<String>,
    /// 内部服务端口
    pub internal_port: u16,
    /// 会话ID
    pub session_id: String,
}

/// 容器基本信息（用于 API 响应）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerBasicInfo {
    /// 容器唯一标识ID
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 容器IP地址
    pub container_ip: String,
    /// 容器内部服务端口
    pub internal_port: u16,
    /// 容器外部映射端口
    pub external_port: u16,
    /// 项目ID
    pub project_id: String,
    /// 会话ID
    pub session_id: String,
    /// 容器状态
    pub status: String,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 服务URL
    pub service_url: String,
}

/// 容器状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ContainerStatus {
    /// 创建中
    Creating,
    /// 运行中
    Running,
    /// 已停止
    Stopped,
    /// 已暂停
    Paused,
    /// 重启中
    Restarting,
    /// 移除中
    Removing,
    /// 已退出
    Exited,
    /// 已死亡
    Dead,
    /// 未知状态
    Unknown(String),
}

impl From<String> for ContainerStatus {
    fn from(status: String) -> Self {
        match status.to_lowercase().as_str() {
            "created" => ContainerStatus::Creating,
            "running" => ContainerStatus::Running,
            "stopped" => ContainerStatus::Stopped,
            "paused" => ContainerStatus::Paused,
            "restarting" => ContainerStatus::Restarting,
            "removing" => ContainerStatus::Removing,
            "exited" => ContainerStatus::Exited,
            "dead" => ContainerStatus::Dead,
            _ => ContainerStatus::Unknown(status),
        }
    }
}

impl ToString for ContainerStatus {
    fn to_string(&self) -> String {
        match self {
            ContainerStatus::Creating => "created".to_string(),
            ContainerStatus::Running => "running".to_string(),
            ContainerStatus::Stopped => "stopped".to_string(),
            ContainerStatus::Paused => "paused".to_string(),
            ContainerStatus::Restarting => "restarting".to_string(),
            ContainerStatus::Removing => "removing".to_string(),
            ContainerStatus::Exited => "exited".to_string(),
            ContainerStatus::Dead => "dead".to_string(),
            ContainerStatus::Unknown(s) => s.clone(),
        }
    }
}

/// Docker 管理器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerManagerConfig {
    /// Docker 守护进程地址
    pub docker_host: Option<String>,
    /// 默认镜像
    pub default_image: String,
    /// 默认网络模式
    pub default_network_mode: String,
    /// 默认工作目录
    pub default_work_dir: String,
    /// 是否启用自动清理
    pub auto_cleanup: bool,
    /// 容器存活时间 (秒)
    pub container_ttl_seconds: Option<u64>,
}

impl Default for DockerManagerConfig {
    fn default() -> Self {
        Self {
            docker_host: None, // 使用默认的 Docker socket
            default_image: crate::DEFAULT_DOCKER_IMAGE.to_string(),
            default_network_mode: crate::DEFAULT_NETWORK_MODE.to_string(),
            default_work_dir: crate::DEFAULT_WORK_DIR.to_string(),
            auto_cleanup: true,
            container_ttl_seconds: Some(3600), // 1小时
        }
    }
}
