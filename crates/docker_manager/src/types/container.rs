//! 容器配置与查询结果类型。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use super::ContainerStatus;

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
    /// 资源限制（复用 shared_types::ServiceResourceLimits，全仓库资源类型统一）
    pub resource_limits: Option<shared_types::ServiceResourceLimits>,
    /// 额外的挂载点
    pub extra_mounts: Vec<MountPoint>,
    /// 启动命令
    pub command: Option<Vec<String>>,
    /// 入口点 (覆盖镜像默认入口点)
    pub entrypoint: Option<Vec<String>>,
    /// 网络名称 (可选，如果不指定则使用默认的 RCODER_NETWORK_NAME)
    pub network_name: Option<String>,

    // === 新增字段 (隔离类型支持) ===
    /// 容器唯一标识（可选），用于容器复用
    /// 若传值，则使用此 ID 作为容器标识，优先级高于 project_id/user_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_id: Option<String>,
    /// 租户 ID（可选），用于多租户场景下的数据隔离
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// 空间 ID（可选），用于区分租户下的不同空间
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space_id: Option<String>,
    /// 隔离类型（可选），控制容器共享粒度：tenant/space/project
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation_type: Option<String>,
    /// 容器安全配置（可选，仅 Docker 模式生效），透传到 bollard HostConfig
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<shared_types::ServiceSecurityConfig>,
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

impl DockerContainerConfig {
    /// 为指定服务类型创建配置
    ///
    /// 使用服务类型动态获取容器名称前缀，避免硬编码
    ///
    /// # Arguments
    ///
    /// * `service_type` - 服务类型（RCoder 或 ComputerAgentRunner）
    ///
    /// # Returns
    ///
    /// 返回配置了正确容器前缀的 DockerContainerConfig
    ///
    /// # Examples
    ///
    /// ```
    /// use docker_manager::DockerContainerConfig;
    /// use shared_types::ServiceType;
    ///
    /// let config = DockerContainerConfig::new_for_service(ServiceType::WebAgentRunner);
    /// assert_eq!(config.name_prefix, "web-agent-runner");
    ///
    /// let config = DockerContainerConfig::new_for_service(ServiceType::ComputerAgentRunner);
    /// assert_eq!(config.name_prefix, "computer-agent-runner");
    /// ```
    pub fn new_for_service(service_type: shared_types::ServiceType) -> Self {
        Self {
            project_id: String::new(),
            image: crate::default_docker_image(),
            name_prefix: service_type.container_prefix().to_string(), // 🔧 动态获取
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
            entrypoint: None,
            network_name: None,
            // 新增字段（隔离类型支持）
            pod_id: None,
            tenant_id: None,
            space_id: None,
            isolation_type: None,
            security: None,
        }
    }
}

impl Default for DockerContainerConfig {
    fn default() -> Self {
        // 默认使用 RCoder 服务
        Self::new_for_service(shared_types::ServiceType::WebAgentRunner)
    }
}

/// 容器实时查询结果
///
/// 用于 `find_project_container` 和 `find_user_container` 的返回值，
/// 提供容器的基本实时信息。
#[derive(Debug, Clone)]
pub struct ContainerQueryResult {
    /// 容器 ID
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 容器状态
    pub status: ContainerStatus,
    /// 是否正在运行
    pub is_running: bool,
    /// 容器 IP 地址（用于 gRPC 健康检查）
    pub container_ip: String,
    /// 容器创建时间
    pub created_at: DateTime<Utc>,
}

/// 容器实时查询结果的 Arc 包装（用于缓存）
pub type ContainerQueryResultArc = Arc<ContainerQueryResult>;

impl ContainerQueryResult {
    /// 创建新的查询结果
    pub fn new(
        container_id: String,
        container_name: String,
        status: ContainerStatus,
        is_running: bool,
        container_ip: String,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            container_id,
            container_name,
            status,
            is_running,
            container_ip,
            created_at,
        }
    }

    /// 从元组创建（用于兼容旧代码）
    pub fn from_tuple(tuple: (String, String, ContainerStatus, bool)) -> Self {
        Self {
            container_id: tuple.0,
            container_name: tuple.1,
            status: tuple.2,
            is_running: tuple.3,
            container_ip: String::new(), // 默认为空，需要后续更新
            created_at: Utc::now(),      // 兼容旧代码，使用当前时间
        }
    }

    /// 创建带有 IP 地址的查询结果（完整构造）
    pub fn with_ip(
        container_id: String,
        container_name: String,
        status: ContainerStatus,
        is_running: bool,
        container_ip: String,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            container_id,
            container_name,
            status,
            is_running,
            container_ip,
            created_at,
        }
    }

    /// 转换为元组（用于兼容旧代码）
    pub fn to_tuple(&self) -> (String, String, ContainerStatus, bool) {
        (
            self.container_id.clone(),
            self.container_name.clone(),
            self.status.clone(),
            self.is_running,
        )
    }
}

/// Docker 容器信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerContainerInfo {
    /// 容器 ID
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 项目 ID（RCoder 模式的主键）
    pub project_id: String,
    /// 用户 ID（ComputerAgentRunner 模式的主键，可选）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// 服务类型（RCoder 或 ComputerAgentRunner）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_type: Option<shared_types::ServiceType>,
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
    /// 🆕 服务层健康状态（gRPC/HTTP 检查结果）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_health: Option<crate::health::ServiceHealthStatus>,
    /// 内部服务端口
    pub internal_port: u16,
    /// 网络名称
    pub network_name: String,
}

impl DockerContainerInfo {
    /// 创建基础容器信息（仅必填字段，其余使用默认值）
    pub fn new(
        container_id: String,
        container_name: String,
        project_id: String,
        image: String,
    ) -> Self {
        Self {
            container_id,
            container_name,
            project_id,
            user_id: None,
            service_type: None,
            image,
            status: ContainerStatus::Running,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            host_path: String::new(),
            container_path: String::new(),
            port_bindings: HashMap::new(),
            assigned_port: 0,
            health_status: None,
            service_health: None,
            internal_port: shared_types::GRPC_DEFAULT_PORT,
            network_name: String::new(),
        }
    }

    /// 获取容器的业务主键
    ///
    /// 根据 `service_type` 返回正确的标识符：
    /// - **RCoder**: 返回 `project_id`
    /// - **ComputerAgentRunner**: 返回 `user_id`（如果有），否则回退到 `project_id`
    ///
    /// # Returns
    /// 容器的业务标识符
    pub fn container_key(&self) -> &str {
        match self.service_type {
            Some(shared_types::ServiceType::ComputerAgentRunner) => {
                // ComputerAgentRunner 模式优先使用 user_id
                self.user_id.as_deref().unwrap_or(&self.project_id)
            }
            Some(shared_types::ServiceType::WebAgentRunner) => {
                // RCoder 模式使用 project_id
                &self.project_id
            }
            _ => {
                // 未知类型使用 project_id
                &self.project_id
            }
        }
    }

    /// 判断是否为 ComputerAgentRunner 容器
    pub fn is_computer_agent(&self) -> bool {
        matches!(
            self.service_type,
            Some(shared_types::ServiceType::ComputerAgentRunner)
        )
    }
}

/// 容器基本信息（使用shared_types中的定义）
pub type ContainerBasicInfo = shared_types::ContainerBasicInfo;
