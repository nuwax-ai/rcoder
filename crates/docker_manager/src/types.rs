use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
    /// 入口点 (覆盖镜像默认入口点)
    pub entrypoint: Option<Vec<String>>,
    /// 网络名称 (可选，如果不指定则使用默认的 RCODER_NETWORK_NAME)
    pub network_name: Option<String>,
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
    /// let config = DockerContainerConfig::new_for_service(ServiceType::RCoder);
    /// assert_eq!(config.name_prefix, "rcoder-agent");
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
        }
    }
}

impl Default for DockerContainerConfig {
    fn default() -> Self {
        // 默认使用 RCoder 服务
        Self::new_for_service(shared_types::ServiceType::RCoder)
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
            Some(shared_types::ServiceType::RCoder) => {
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
    /// 默认平台
    pub default_platform: String,
    /// 默认网络模式
    pub default_network_mode: String,
    /// 默认工作目录
    pub default_work_dir: String,
    /// 是否启用自动清理
    pub auto_cleanup: bool,
    /// 容器存活时间 (秒)
    pub container_ttl_seconds: Option<u64>,

    /// 多镜像配置（从 rcoder 配置传递，始终有值）
    pub multi_image_config: shared_types::MultiImageConfig,
}

/// Docker 配置（从 rcoder 配置传递）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerConfig {
    /// Docker 镜像名称（根据架构自动选择）
    pub image: Option<String>,
    /// ARM64 架构的 Docker 镜像
    pub arm64_image: Option<String>,
    /// AMD64 架构的 Docker 镜像
    pub amd64_image: Option<String>,
    /// 默认回退镜像（当无法检测架构或架构不匹配时使用）
    pub default_image: Option<String>,
    /// 默认网络模式
    pub network_mode: Option<String>,
    /// 默认工作目录
    pub work_dir: Option<String>,
    /// 是否启用自动清理
    pub auto_cleanup: Option<bool>,
    /// 容器存活时间（秒）
    pub container_ttl_seconds: Option<u64>,
}

impl Default for DockerManagerConfig {
    fn default() -> Self {
        Self {
            docker_host: None, // 使用默认的 Docker socket
            default_image: crate::default_docker_image(),
            default_platform: crate::default_platform(),
            default_network_mode: crate::DEFAULT_NETWORK_MODE.to_string(),
            default_work_dir: crate::DEFAULT_WORK_DIR.to_string(),
            auto_cleanup: true,
            container_ttl_seconds: Some(3600), // 1小时

            multi_image_config: shared_types::create_default_multi_image_config(), // 默认多镜像配置
        }
    }
}

/// 容器清理结果统计
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CleanupResult {
    /// 找到的容器数量
    pub total_found: usize,
    /// 成功删除的容器数量
    pub successfully_removed: usize,
    /// 删除失败的容器数量
    pub failed_removals: usize,
    /// 跳过的运行中容器数量（仅在非强制删除时）
    pub skipped_running: usize,
    /// 被删除的容器ID列表
    pub removed_container_ids: Vec<String>,
    /// 失败的容器及错误信息
    pub failed_removals_details: Vec<ContainerRemovalFailure>,
    /// 清理操作耗时（毫秒）
    pub duration_ms: u64,
}

impl CleanupResult {
    /// 是否完全成功（没有失败）
    pub fn is_complete_success(&self) -> bool {
        self.failed_removals == 0
    }

    /// 是否有任何成功删除的容器
    pub fn has_removals(&self) -> bool {
        self.successfully_removed > 0
    }

    /// 获取成功率百分比
    pub fn success_rate(&self) -> f64 {
        if self.total_found == 0 {
            100.0
        } else {
            (self.successfully_removed as f64 / self.total_found as f64) * 100.0
        }
    }
}

/// 容器删除失败信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRemovalFailure {
    /// 容器ID
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 失败原因
    pub error_message: String,
}

/// 容器过滤条件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContainerFilter {
    /// 按名称模式过滤
    NamePattern(String),
    /// 按状态过滤
    Status(Vec<ContainerStatus>),
    /// 按标签过滤
    Label(String, String),
    /// 组合过滤条件（AND逻辑）
    And(Vec<ContainerFilter>),
    /// 组合过滤条件（OR逻辑）
    Or(Vec<ContainerFilter>),
}

impl ContainerFilter {
    /// 创建名称模式过滤器
    pub fn name_pattern(pattern: impl Into<String>) -> Self {
        ContainerFilter::NamePattern(pattern.into())
    }

    /// 创建状态过滤器
    pub fn status(statuses: Vec<ContainerStatus>) -> Self {
        ContainerFilter::Status(statuses)
    }

    /// 创建标签过滤器
    pub fn label(key: impl Into<String>, value: impl Into<String>) -> Self {
        ContainerFilter::Label(key.into(), value.into())
    }

    /// 创建AND组合过滤器
    pub fn and(filters: Vec<ContainerFilter>) -> Self {
        ContainerFilter::And(filters)
    }

    /// 创建OR组合过滤器
    pub fn or(filters: Vec<ContainerFilter>) -> Self {
        ContainerFilter::Or(filters)
    }

    /// 检查容器是否匹配过滤条件
    pub fn matches(&self, container: &bollard::models::ContainerSummary) -> bool {
        match self {
            ContainerFilter::NamePattern(pattern) => {
                if let Some(names) = &container.names {
                    for name in names {
                        // Docker 容器名称通常以 '/' 开头，需要去掉
                        let clean_name = name.trim_start_matches('/');
                        if Self::matches_pattern(clean_name, pattern) {
                            return true;
                        }
                    }
                }
                false
            }
            ContainerFilter::Status(statuses) => {
                if let Some(state) = &container.state {
                    // 转换为字符串进行比较
                    let state_str = match state {
                        bollard::models::ContainerSummaryStateEnum::RUNNING => "running",
                        bollard::models::ContainerSummaryStateEnum::EXITED => "exited",
                        bollard::models::ContainerSummaryStateEnum::CREATED => "created",
                        bollard::models::ContainerSummaryStateEnum::PAUSED => "paused",
                        bollard::models::ContainerSummaryStateEnum::RESTARTING => "restarting",
                        bollard::models::ContainerSummaryStateEnum::REMOVING => "removing",
                        bollard::models::ContainerSummaryStateEnum::DEAD => "dead",
                        bollard::models::ContainerSummaryStateEnum::EMPTY => "unknown",
                    };
                    statuses.iter().any(|s| s.to_string() == state_str)
                } else {
                    false
                }
            }
            ContainerFilter::Label(key, value) => {
                if let Some(labels) = &container.labels {
                    labels.get(key.as_str()) == Some(value)
                } else {
                    false
                }
            }
            ContainerFilter::And(filters) => filters.iter().all(|f| f.matches(container)),
            ContainerFilter::Or(filters) => filters.iter().any(|f| f.matches(container)),
        }
    }

    /// 简单的模式匹配（支持 * 通配符）
    fn matches_pattern(text: &str, pattern: &str) -> bool {
        // 如果模式不包含通配符，直接比较
        if !pattern.contains('*') {
            return text == pattern;
        }

        // 简单的通配符匹配实现
        Self::wildcard_match(text, pattern)
    }

    /// 通配符匹配实现
    fn wildcard_match(text: &str, pattern: &str) -> bool {
        let pattern_chars: Vec<char> = pattern.chars().collect();
        let text_chars: Vec<char> = text.chars().collect();

        let mut text_idx = 0;
        let mut pattern_idx = 0;
        let mut star_idx = -1isize;
        let mut match_idx = 0;

        while text_idx < text_chars.len() {
            if pattern_idx < pattern_chars.len()
                && (pattern_chars[pattern_idx] == text_chars[text_idx]
                    || pattern_chars[pattern_idx] == '?')
            {
                text_idx += 1;
                pattern_idx += 1;
            } else if pattern_idx < pattern_chars.len() && pattern_chars[pattern_idx] == '*' {
                star_idx = pattern_idx as isize;
                match_idx = text_idx as isize;
                pattern_idx += 1;
            } else if star_idx != -1 {
                pattern_idx = (star_idx + 1) as usize;
                match_idx += 1;
                text_idx = match_idx as usize;
            } else {
                return false;
            }
        }

        // 处理模式末尾的通配符
        while pattern_idx < pattern_chars.len() && pattern_chars[pattern_idx] == '*' {
            pattern_idx += 1;
        }

        pattern_idx == pattern_chars.len() && text_idx == text_chars.len()
    }
}

/// 容器清理选项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupOptions {
    /// 是否强制删除运行中的容器
    pub force_remove_running: bool,
    /// 是否等待容器优雅停止
    pub wait_for_graceful_stop: bool,
    /// 优雅停止超时时间（秒）
    pub stop_timeout_seconds: u64,
    /// 删除容器后是否同时清理相关卷
    pub remove_associated_volumes: bool,
}

impl Default for CleanupOptions {
    fn default() -> Self {
        Self {
            force_remove_running: false,
            wait_for_graceful_stop: true,
            stop_timeout_seconds: 30,
            remove_associated_volumes: false,
        }
    }
}
