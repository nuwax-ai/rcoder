//! 服务镜像配置
//!
//! 定义了每个服务类型的镜像配置、环境变量、挂载点等信息。

use crate::service_type::ServiceType;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 容器路径模板的默认值
fn default_container_path_template() -> String {
    "/app/project_workspace/{project_id}".to_string()
}

/// Computer Agent Runner 容器路径模板的默认值
fn default_computer_agent_runner_container_path_template() -> String {
    "/app/computer-project-workspace/{user_id}/{project_id}".to_string()
}

/// 服务镜像配置
///
/// 定义了每个服务类型的详细配置，包括镜像选择、环境变量和挂载点。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceImageConfig {
    /// 服务类型
    pub service_type: ServiceType,
    /// 通用镜像（优先级最高，如果指定则忽略架构特定镜像）
    pub image: Option<String>,
    /// ARM64 架构专用镜像
    pub arm64_image: Option<String>,
    /// AMD64 架构专用镜像
    pub amd64_image: Option<String>,
    /// 默认回退镜像
    pub default_image: Option<String>,
    /// 镜像标签前缀（用于自动构建镜像名称）
    pub image_tag_prefix: Option<String>,
    /// 是否启用该服务类型
    pub enabled: bool,
    /// 服务特定的环境变量
    pub environment: HashMap<String, String>,
    /// 服务特定的挂载点
    pub mounts: Vec<ServiceMountConfig>,
    /// 容器启动命令
    pub command: Vec<String>,
    /// 容器入口点
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    /// 容器资源限制配置
    pub resource_limits: ServiceResourceLimits,
    /// 容器工作目录
    pub work_dir: String,
    /// 容器网络模式
    pub network_mode: String,
    /// 容器内挂载路径模板（支持变量替换）
    /// 默认值: "/app/project_workspace/{project_id}"
    /// 支持变量: {project_id}, {user_id}, {service_type}
    #[serde(default = "default_container_path_template")]
    pub container_path_template: String,
    /// rcoder 容器内用于反向解析宿主机路径的基准路径
    ///
    /// DockerManager 通过此路径调用 Docker API 解析出宿主机绝对路径，用于构建挂载。
    /// 未配置时自动从 container_path_template 截取 `{` 前缀推导：
    ///   - RCoder: "/app/project_workspace/{project_id}" → "/app/project_workspace"
    ///   - ComputerAgentRunner: "/app/computer-project-workspace/{user_id}/{project_id}" → "/app/computer-project-workspace"
    #[serde(default)]
    pub workspace_resolution_path: Option<String>,
}

/// 服务挂载点配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceMountConfig {
    /// 容器内路径
    pub container_path: String,
    /// 宿主机路径（支持变量替换）
    /// 可使用的变量：
    /// - {resolved_path}: 从 resolve_from 解析后的宿主机基础路径
    /// - {project_id}: 项目 ID
    /// - {user_id}: 用户 ID
    /// - {container_name}: 容器名称
    /// - {timestamp}: 时间戳（YYYYMMDDHHMMSS 格式）
    /// - {log_dir_name}: 日志目录名（container_name-timestamp）
    ///
    /// 示例: "{resolved_path}/{log_dir_name}" => "/host/logs/computer-agent-runner-user_123-20241212160000"
    pub host_path: String,
    /// 是否只读
    pub read_only: bool,
    /// 挂载类型（bind/volume）
    pub mount_type: String,
    /// 动态路径解析源（可选）
    /// 当 host_path 包含 {resolved_path} 变量时，指定从哪个容器内路径解析宿主机基础路径
    /// 例如：resolve_from: "/app/logs" 会将容器内的 /app/logs 解析为宿主机绝对路径
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolve_from: Option<String>,
}

/// 服务资源限制配置
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServiceResourceLimits {
    /// 内存限制（字节，支持浮点数输入）
    pub memory_limit: Option<f64>,
    /// CPU 限制（核心数）
    pub cpu_limit: Option<f64>,
    /// 交换空间限制（字节，支持浮点数输入）
    pub swap_limit: Option<f64>,
}

impl ServiceResourceLimits {
    /// 验证资源限制的合理性
    pub fn validate(&self) -> Result<(), String> {
        // 内存限制：512MB ~ 64GB
        if let Some(memory) = self.memory_limit {
            if memory < 512_000_000.0 {
                return Err("memory_limit must be at least 512MB".to_string());
            }
            if memory > 64_000_000_000.0 {
                return Err("memory_limit cannot exceed 64GB".to_string());
            }
        }

        // CPU 限制：0.5 ~ 32 核
        if let Some(cpu) = self.cpu_limit {
            if cpu < 0.5 {
                return Err("cpu_limit must be at least 0.5 cores".to_string());
            }
            if cpu > 32.0 {
                return Err("cpu_limit cannot exceed 32 cores".to_string());
            }
        }

        // Swap 应该 >= 内存
        if let (Some(memory), Some(swap)) = (self.memory_limit, self.swap_limit)
            && swap < memory
        {
            return Err("swap_limit should be >= memory_limit".to_string());
        }

        Ok(())
    }

    /// 合并资源限制（override_limits 覆盖 self 中的字段）
    pub fn merge_with(&self, override_limits: &ServiceResourceLimits) -> Self {
        Self {
            memory_limit: override_limits.memory_limit.or(self.memory_limit),
            cpu_limit: override_limits.cpu_limit.or(self.cpu_limit),
            swap_limit: override_limits.swap_limit.or(self.swap_limit),
        }
    }
}

/// 验证结果
#[derive(Debug)]
pub enum ConfigValidationResult {
    Valid,
    Warning(String),
    Error(String),
}

impl ServiceImageConfig {
    /// 验证服务镜像配置的有效性
    pub fn validate(&self) -> ConfigValidationResult {
        // 验证至少有一个镜像配置
        if self.image.is_none()
            && self.arm64_image.is_none()
            && self.amd64_image.is_none()
            && self.default_image.is_none()
        {
            return ConfigValidationResult::Error(format!(
                "Service type {} must have at least one image configured",
                self.service_type
            ));
        }

        // 验证镜像名称格式
        for image in [
            &self.image,
            &self.arm64_image,
            &self.amd64_image,
            &self.default_image,
        ]
        .into_iter()
        .flatten()
        {
            if image.trim().is_empty() {
                return ConfigValidationResult::Warning(format!(
                    "Service type {} has empty image name",
                    self.service_type
                ));
            }

            // 验证镜像名称格式（简单的格式检查）
            if !image
                .chars()
                .all(|c: char| c.is_alphanumeric() || "/:.-_".contains(c))
            {
                return ConfigValidationResult::Warning(format!(
                    "Service type {} image name '{}' may contain invalid characters",
                    self.service_type, image
                ));
            }
        }

        // 验证挂载点配置
        for mount in &self.mounts {
            if mount.container_path.trim().is_empty() {
                return ConfigValidationResult::Error(format!(
                    "Service type {} has empty container mount path",
                    self.service_type
                ));
            }

            if mount.host_path.trim().is_empty() {
                return ConfigValidationResult::Error(format!(
                    "Service type {} has empty host mount path",
                    self.service_type
                ));
            }

            // 验证挂载类型
            if mount.mount_type != "bind" && mount.mount_type != "volume" {
                return ConfigValidationResult::Warning(format!(
                    "Service type {} has unsupported mount type '{}'",
                    self.service_type, mount.mount_type
                ));
            }
        }

        ConfigValidationResult::Valid
    }

    /// 根据当前平台选择合适的镜像
    pub fn get_image_for_platform(&self, platform: &str) -> Option<String> {
        // 优先使用通用镜像
        if let Some(ref image) = self.image {
            return Some(image.clone());
        }

        // 根据平台选择架构特定镜像
        match platform {
            "linux/arm64" => self
                .arm64_image
                .clone()
                .or_else(|| self.default_image.clone()),
            "linux/amd64" => self
                .amd64_image
                .clone()
                .or_else(|| self.default_image.clone()),
            _ => {
                tracing::warn!("Unknown platform '{}', using default image", platform);
                self.default_image.clone()
            }
        }
    }

    /// 合并环境变量（基础环境 + 服务特定环境）
    pub fn merge_environment(&self, base_env: &HashMap<String, String>) -> HashMap<String, String> {
        let mut merged = base_env.clone();
        merged.extend(self.environment.clone());
        merged
    }

    /// 获取挂载点的字符串表示
    pub fn get_mounts_description(&self) -> String {
        if self.mounts.is_empty() {
            return "No mount points".to_string();
        }

        self.mounts
            .iter()
            .map(|mount| {
                format!(
                    "{} -> {} ({})",
                    mount.host_path, mount.container_path, mount.mount_type
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// 获取配置摘要
    pub fn get_summary(&self) -> String {
        format!(
            "ServiceType: {}, Enabled: {}, Image: {:?}, Mounts: {}",
            self.service_type,
            self.enabled,
            self.image
                .as_ref()
                .or(self.arm64_image.as_ref())
                .or(self.amd64_image.as_ref())
                .or(self.default_image.as_ref()),
            self.get_mounts_description()
        )
    }

    /// 获取容器名称前缀
    ///
    /// 优先使用配置的 image_tag_prefix，否则使用 service_type 的默认前缀。
    /// 这确保了容器创建和清理时使用一致的前缀。
    ///
    /// # Returns
    ///
    /// 容器名称前缀字符串
    pub fn container_prefix(&self) -> &str {
        self.image_tag_prefix
            .as_deref()
            .unwrap_or_else(|| self.service_type.container_prefix())
    }

    /// 获取 workspace 解析路径（rcoder 容器内路径）
    ///
    /// 优先使用显式配置的 workspace_resolution_path，
    /// 未配置时根据 service_type 使用默认值。
    pub fn effective_workspace_resolution_path(&self) -> String {
        self.workspace_resolution_path
            .clone()
            .unwrap_or_else(|| match self.service_type {
                ServiceType::RCoder => "/app/project_workspace".to_string(),
                ServiceType::ComputerAgentRunner => "/app/computer-project-workspace".to_string(),
            })
    }

    /// 获取 workspace 在 sub-container 内的挂载路径
    ///
    /// 从环境变量 `PROJECT_WORKSPACE_BASE` 读取（config.yml 已配置），
    /// 回退到 effective_workspace_resolution_path()。
    ///
    /// - RCoder: `PROJECT_WORKSPACE_BASE="/app/project_workspace"`
    /// - ComputerAgentRunner: `PROJECT_WORKSPACE_BASE="/home/user"`
    pub fn workspace_container_path(&self) -> String {
        self.environment
            .get("PROJECT_WORKSPACE_BASE")
            .cloned()
            .unwrap_or_else(|| self.effective_workspace_resolution_path())
    }

    /// 解析容器路径模板，进行变量替换
    ///
    /// 支持的变量:
    /// - {project_id}: 项目ID
    /// - {user_id}: 用户ID
    /// - {service_type}: 服务类型
    ///
    /// # Arguments
    /// * `variables` - 包含变量名和值的 HashMap
    ///
    /// # Returns
    /// 解析后的容器路径字符串
    ///
    /// # Example
    ///
    /// 替换模板中的变量占位符（如 `{project_id}`）为实际值：
    ///
    /// - 输入模板: `/app/project_workspace/{project_id}`
    /// - 变量: `{"project_id": "123"}`
    /// - 输出: `/app/project_workspace/123`
    ///
    pub fn resolve_container_path(
        &self,
        variables: &std::collections::HashMap<String, String>,
    ) -> String {
        let mut resolved = self.container_path_template.clone();
        for (key, value) in variables {
            resolved = resolved.replace(&format!("{{{}}}", key), value);
        }
        resolved
    }
}

impl ServiceMountConfig {
    /// 验证挂载点配置
    pub fn validate(&self) -> ConfigValidationResult {
        if self.container_path.trim().is_empty() {
            return ConfigValidationResult::Error(
                "Container mount path cannot be empty".to_string(),
            );
        }

        if self.host_path.trim().is_empty() {
            return ConfigValidationResult::Error("Host mount path cannot be empty".to_string());
        }

        // 验证路径格式
        if self.container_path.starts_with('/')
            && !self
                .container_path
                .chars()
                .all(|c: char| c.is_alphanumeric() || "/-_.".contains(c))
        {
            return ConfigValidationResult::Warning(format!(
                "Container mount path '{}' may contain invalid characters",
                self.container_path
            ));
        }

        if self.mount_type != "bind" && self.mount_type != "volume" {
            return ConfigValidationResult::Error(format!(
                "Unsupported mount type '{}', must be 'bind' or 'volume'",
                self.mount_type
            ));
        }

        ConfigValidationResult::Valid
    }

    /// 解析宿主机路径中的变量
    /// 支持的变量：
    /// - {project_id}: 项目ID
    /// - {workspace_dir}: 工作目录
    pub fn resolve_host_path(&self, variables: &HashMap<String, String>) -> String {
        let mut resolved = self.host_path.clone();

        for (key, value) in variables {
            resolved = resolved.replace(&format!("{{{}}}", key), value);
        }

        resolved
    }
}

/// 创建默认的 RCoder 服务配置
pub fn default_rcoder_service_config() -> ServiceImageConfig {
    let mut environment = HashMap::new();
    environment.insert("RUST_LOG".to_string(), "info".to_string());
    environment.insert("SERVICE_MODE".to_string(), "full".to_string());
    environment.insert("API_PORT".to_string(), "8086".to_string());

    // 🔥 默认不提供挂载配置，让配置文件控制
    let mounts = vec![];

    // 默认启动命令
    let command = vec![
        "/app/bin/agent_runner".to_string(),
        "--port".to_string(),
        "8086".to_string(),
    ];

    // 默认资源限制
    let resource_limits = ServiceResourceLimits {
        memory_limit: Some(2_000_000_000.0), // 2GB
        cpu_limit: Some(2.0),                // 2 核
        swap_limit: Some(4_000_000_000.0),   // 4GB
    };

    ServiceImageConfig {
        service_type: ServiceType::RCoder,
        image: None, // 使用架构特定镜像
        arm64_image: Some("registry.yichamao.com/rcoder:latest-arm64".to_string()),
        amd64_image: Some("registry.yichamao.com/rcoder:latest-amd64".to_string()),
        default_image: Some("registry.yichamao.com/rcoder:latest".to_string()),
        image_tag_prefix: Some("rcoder-agent".to_string()),
        enabled: true, // 当前启用
        environment,
        mounts,
        command,
        entrypoint: None, // 使用镜像默认入口点
        resource_limits,
        work_dir: "/app".to_string(),
        network_mode: "bridge".to_string(),
        container_path_template: default_container_path_template(),
        workspace_resolution_path: None,
    }
}

/// 创建默认的 Computer Agent Runner 服务配置
pub fn default_agent_runner_service_config() -> ServiceImageConfig {
    let mut environment = HashMap::new();
    environment.insert("RUST_LOG".to_string(), "debug".to_string());
    environment.insert("SERVICE_MODE".to_string(), "agent-only".to_string());
    environment.insert("AGENT_PORT".to_string(), "8086".to_string());
    environment.insert(
        "PROJECT_WORKSPACE_BASE".to_string(),
        "/home/user".to_string(),
    );

    // 🔥 Agent 清理配置（通过环境变量控制）
    // 设置为 3600 秒（1小时），用户可以在 docker/config.yml 中覆盖此值
    environment.insert(
        "RCODER_AGENT_IDLE_TIMEOUT_SECS".to_string(),
        "3600".to_string(),
    ); // 1 小时

    let mounts = vec![];

    // 默认启动命令
    let command = vec![
        "/app/bin/agent_runner".to_string(),
        "--port".to_string(),
        "8086".to_string(),
    ];

    // 默认资源限制（ComputerAgentRunner 可能需要更多资源）
    let resource_limits = ServiceResourceLimits {
        memory_limit: Some(4_000_000_000.0), // 4GB
        cpu_limit: Some(3.0),                // 3 核
        swap_limit: Some(8_000_000_000.0),   // 8GB
    };

    ServiceImageConfig {
        service_type: ServiceType::ComputerAgentRunner,
        image: None, // 使用架构特定镜像
        arm64_image: Some(
            "registry.yichamao.com/rcoder-computer-agent-runner:latest-arm64".to_string(),
        ),
        amd64_image: Some(
            "registry.yichamao.com/rcoder-computer-agent-runner:latest-amd64".to_string(),
        ),
        default_image: Some(
            "registry.yichamao.com/rcoder-computer-agent-runner:latest".to_string(),
        ),
        image_tag_prefix: Some("rcoder-computer-agent-runner".to_string()),
        enabled: true, // 当前启用
        environment,
        mounts,
        command,
        entrypoint: None, // 使用镜像默认入口点
        resource_limits,
        work_dir: "/app".to_string(),
        network_mode: "bridge".to_string(),
        container_path_template: default_computer_agent_runner_container_path_template(),
        workspace_resolution_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_config_validation() {
        let config = default_rcoder_service_config();

        // 有效配置
        assert!(matches!(config.validate(), ConfigValidationResult::Valid));

        // 无效配置：所有镜像为空
        let mut invalid_config = config.clone();
        invalid_config.image = None;
        invalid_config.arm64_image = None;
        invalid_config.amd64_image = None;
        invalid_config.default_image = None;
        assert!(matches!(
            invalid_config.validate(),
            ConfigValidationResult::Error(_)
        ));
    }

    #[test]
    fn test_environment_merge() {
        let config = default_rcoder_service_config();

        let mut base_env = HashMap::new();
        base_env.insert("BASE_VAR".to_string(), "base_value".to_string());
        base_env.insert("RUST_LOG".to_string(), "debug".to_string()); // 重叠

        let merged = config.merge_environment(&base_env);

        assert_eq!(merged.get("BASE_VAR"), Some(&"base_value".to_string()));
        // 服务特定环境变量应该覆盖基础变量
        assert_eq!(merged.get("RUST_LOG"), Some(&"info".to_string())); // RCoder 配置是 info
        assert_eq!(merged.get("SERVICE_MODE"), Some(&"full".to_string()));
    }

    #[test]
    fn test_mount_validation() {
        // 创建一个有挂载点的配置用于测试
        let config_with_mounts = ServiceImageConfig {
            service_type: ServiceType::RCoder,
            image: None,
            arm64_image: Some("test-image:arm64".to_string()),
            amd64_image: Some("test-image:amd64".to_string()),
            default_image: Some("test-image:latest".to_string()),
            image_tag_prefix: None,
            enabled: true,
            environment: HashMap::new(),
            mounts: vec![ServiceMountConfig {
                container_path: "/app/workspace".to_string(),
                host_path: "/host/workspace".to_string(),
                read_only: false,
                mount_type: "bind".to_string(),
                resolve_from: None,
            }],
            command: vec![],
            entrypoint: None,
            resource_limits: ServiceResourceLimits {
                memory_limit: None,
                cpu_limit: None,
                swap_limit: None,
            },
            work_dir: "/app".to_string(),
            network_mode: "bridge".to_string(),
            container_path_template: "/app/project_workspace/{project_id}".to_string(),
            workspace_resolution_path: None,
        };

        for mount in &config_with_mounts.mounts {
            assert!(matches!(mount.validate(), ConfigValidationResult::Valid));
        }

        // 测试无效挂载
        let mut invalid_mount = config_with_mounts.mounts[0].clone();
        invalid_mount.container_path = "".to_string();
        assert!(matches!(
            invalid_mount.validate(),
            ConfigValidationResult::Error(_)
        ));
    }

    #[test]
    fn test_mount_path_resolution() {
        let mut variables = HashMap::new();
        variables.insert("project_id".to_string(), "test-project-123".to_string());
        variables.insert("workspace_dir".to_string(), "/app/workspace".to_string());

        let mount = ServiceMountConfig {
            container_path: "/app/workspace/{project_id}".to_string(),
            host_path: "{workspace_dir}/projects/{project_id}".to_string(),
            read_only: false,
            mount_type: "bind".to_string(),
            resolve_from: None,
        };

        let resolved = mount.resolve_host_path(&variables);
        assert_eq!(resolved, "/app/workspace/projects/test-project-123");
    }

    #[test]
    fn test_get_summary() {
        let config = default_rcoder_service_config();
        let summary = config.get_summary();

        assert!(summary.contains("rcoder"));
        assert!(summary.contains("Enabled: true"));
        assert!(summary.contains("registry.yichamao.com/rcoder"));
    }

    #[test]
    fn test_container_prefix_with_image_tag_prefix() {
        // 测试使用 image_tag_prefix 的情况
        let config = default_agent_runner_service_config();
        assert_eq!(config.container_prefix(), "rcoder-computer-agent-runner");
    }

    #[test]
    fn test_container_prefix_fallback_to_service_type() {
        // 测试没有 image_tag_prefix 时回退到 service_type 默认值
        let mut config = default_rcoder_service_config();
        config.image_tag_prefix = None;
        assert_eq!(config.container_prefix(), "rcoder-agent");
    }

    #[test]
    fn test_container_prefix_rcoder() {
        // RCoder 配置使用 rcoder-agent 前缀
        let config = default_rcoder_service_config();
        assert_eq!(config.container_prefix(), "rcoder-agent");
    }

    /// 测试 ServiceType::container_prefix() 与 ServiceConfig::container_prefix() 的差异
    ///
    /// 这是导致 VNC 状态查询返回 CONTAINER_NOT_FOUND 的根因：
    /// - ServiceType::container_prefix() 返回硬编码的 "computer-agent-runner"
    /// - ServiceConfig::container_prefix() 读取配置的 image_tag_prefix "rcoder-computer-agent-runner"
    /// - 容器创建使用后者，而错误的查询代码使用前者，导致名称不匹配
    #[test]
    fn test_container_prefix_difference_causes_container_not_found() {
        // 硬编码的 ServiceType 前缀（错误的查询方式）
        let service_type_prefix = ServiceType::ComputerAgentRunner.container_prefix();
        assert_eq!(service_type_prefix, "computer-agent-runner");

        // 配置化的 ServiceConfig 前缀（正确的创建方式）
        let config = default_agent_runner_service_config();
        let config_prefix = config.container_prefix();
        assert_eq!(config_prefix, "rcoder-computer-agent-runner");

        // 明确展示差异：两者不同！
        assert_ne!(
            service_type_prefix, config_prefix,
            "ServiceType::container_prefix() 与 ServiceConfig::container_prefix() 应该不同"
        );

        // 展示如果用错误的前缀构造容器名会导致什么问题
        let user_id = "1743762321";
        let wrong_container_name = format!("{}-{}", service_type_prefix, user_id);
        let correct_container_name = format!("{}-{}", config_prefix, user_id);

        assert_eq!(wrong_container_name, "computer-agent-runner-1743762321");
        assert_eq!(
            correct_container_name,
            "rcoder-computer-agent-runner-1743762321"
        );

        // 如果用错误的名字去查询，当然找不到正确名字创建的容器
        assert_ne!(wrong_container_name, correct_container_name);
    }

    #[test]
    fn test_resource_limits_validation_valid() {
        let valid = ServiceResourceLimits {
            memory_limit: Some(1_000_000_000.0), // 1GB
            cpu_limit: Some(2.0),
            swap_limit: Some(2_000_000_000.0), // 2GB
        };
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn test_resource_limits_validation_invalid_memory_too_small() {
        let invalid = ServiceResourceLimits {
            memory_limit: Some(256_000_000.0), // 256MB - 太小
            cpu_limit: None,
            swap_limit: None,
        };
        assert!(invalid.validate().is_err());
        assert!(invalid.validate().unwrap_err().contains("at least 512MB"));
    }

    #[test]
    fn test_resource_limits_validation_invalid_memory_too_large() {
        let invalid = ServiceResourceLimits {
            memory_limit: Some(100_000_000_000.0), // 100GB - 太大
            cpu_limit: None,
            swap_limit: None,
        };
        assert!(invalid.validate().is_err());
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .contains("cannot exceed 64GB")
        );
    }

    #[test]
    fn test_resource_limits_validation_invalid_cpu_too_small() {
        let invalid = ServiceResourceLimits {
            memory_limit: None,
            cpu_limit: Some(0.1), // 太小
            swap_limit: None,
        };
        assert!(invalid.validate().is_err());
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .contains("at least 0.5 cores")
        );
    }

    #[test]
    fn test_resource_limits_validation_invalid_swap_less_than_memory() {
        let invalid = ServiceResourceLimits {
            memory_limit: Some(2_000_000_000.0), // 2GB
            cpu_limit: None,
            swap_limit: Some(1_000_000_000.0), // 1GB - swap < memory
        };
        assert!(invalid.validate().is_err());
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .contains("should be >= memory_limit")
        );
    }

    #[test]
    fn test_resource_limits_merge() {
        let default_limits = ServiceResourceLimits {
            memory_limit: Some(2_000_000_000.0), // 2GB
            cpu_limit: Some(2.0),
            swap_limit: Some(4_000_000_000.0), // 4GB
        };

        let override_limits = ServiceResourceLimits {
            memory_limit: Some(4_000_000_000.0), // 覆盖：4GB
            cpu_limit: None,                     // 不覆盖
            swap_limit: Some(8_000_000_000.0),   // 覆盖：8GB
        };

        let merged = default_limits.merge_with(&override_limits);
        assert_eq!(merged.memory_limit, Some(4_000_000_000.0));
        assert_eq!(merged.cpu_limit, Some(2.0)); // 保留默认
        assert_eq!(merged.swap_limit, Some(8_000_000_000.0));
    }

    #[test]
    fn test_resource_limits_merge_all_none() {
        let default_limits = ServiceResourceLimits {
            memory_limit: Some(2_000_000_000.0), // 2GB
            cpu_limit: Some(2.0),
            swap_limit: Some(4_000_000_000.0), // 4GB
        };

        let override_limits = ServiceResourceLimits {
            memory_limit: None,
            cpu_limit: None,
            swap_limit: None,
        };

        let merged = default_limits.merge_with(&override_limits);
        // 所有字段都应该保留默认值
        assert_eq!(merged.memory_limit, Some(2_000_000_000.0));
        assert_eq!(merged.cpu_limit, Some(2.0));
        assert_eq!(merged.swap_limit, Some(4_000_000_000.0));
    }

    #[test]
    fn test_workspace_resolution_path_rcoder() {
        let config = default_rcoder_service_config();
        // 未显式配置时，从 container_path_template 推导
        assert_eq!(
            config.effective_workspace_resolution_path(),
            "/app/project_workspace"
        );
    }

    #[test]
    fn test_workspace_resolution_path_computer_agent_runner() {
        let config = default_agent_runner_service_config();
        // 未显式配置时，从 container_path_template 推导
        assert_eq!(
            config.effective_workspace_resolution_path(),
            "/app/computer-project-workspace"
        );
    }

    #[test]
    fn test_workspace_resolution_path_explicit_override() {
        let mut config = default_rcoder_service_config();
        config.workspace_resolution_path = Some("/custom/path".to_string());
        assert_eq!(config.effective_workspace_resolution_path(), "/custom/path");
    }

    #[test]
    fn test_workspace_container_path_rcoder() {
        let config = default_rcoder_service_config();
        // RCoder: PROJECT_WORKSPACE_BASE="/app/project_workspace"
        assert_eq!(config.workspace_container_path(), "/app/project_workspace");
    }

    #[test]
    fn test_workspace_container_path_computer_agent_runner() {
        let config = default_agent_runner_service_config();
        // ComputerAgentRunner: PROJECT_WORKSPACE_BASE="/home/user"
        assert_eq!(config.workspace_container_path(), "/home/user");
    }

    #[test]
    fn test_workspace_container_path_fallback() {
        let mut config = default_rcoder_service_config();
        config.environment.remove("PROJECT_WORKSPACE_BASE");
        // 无环境变量时回退到 effective_workspace_resolution_path
        assert_eq!(
            config.workspace_container_path(),
            config.effective_workspace_resolution_path()
        );
    }
}
