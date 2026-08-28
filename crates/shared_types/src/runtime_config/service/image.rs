//! 服务镜像/挂载配置与校验。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::defaults::{default_container_path_template, default_network_mode, default_work_dir};
use super::resource::{ServiceResourceLimits, ServiceSecurityConfig};
use crate::ServiceType;

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
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// 服务特定的挂载点
    #[serde(default)]
    pub mounts: Vec<ServiceMountConfig>,
    /// 容器启动命令
    #[serde(default)]
    pub command: Vec<String>,
    /// 容器入口点
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    /// 容器资源限制配置
    #[serde(default)]
    pub resource_limits: ServiceResourceLimits,
    /// 容器工作目录
    #[serde(default = "default_work_dir")]
    pub work_dir: String,
    /// 容器网络模式
    #[serde(default = "default_network_mode")]
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
    /// 容器安全配置（可选）。仅 Docker 部署模式透传到 bollard HostConfig；
    /// 未配置（None）时走代码默认（privileged=false + `cap_drop=[NET_RAW, NET_ADMIN]`）。
    #[serde(default)]
    pub security: Option<ServiceSecurityConfig>,
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
            tracing::error!(
                "[CONFIG_VALIDATION] Service type {} has no image configured! \
                 image={:?}, arm64_image={:?}, amd64_image={:?}, default_image={:?}, \
                 image_tag_prefix={:?}, enabled={}",
                self.service_type,
                self.image,
                self.arm64_image,
                self.amd64_image,
                self.default_image,
                self.image_tag_prefix,
                self.enabled
            );
            return ConfigValidationResult::Error(format!(
                "Service type {} must have at least one image configured (image={:?}, arm64={:?}, amd64={:?}, default={:?})",
                self.service_type,
                self.image,
                self.arm64_image,
                self.amd64_image,
                self.default_image
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
                ServiceType::WebAgentRunner => crate::paths::WORKSPACE_ROOT.to_string(),
                ServiceType::ComputerAgentRunner => {
                    crate::paths::COMPUTER_WORKSPACE_ROOT.to_string()
                }
                // UserApp 复用 rcoder-workspace PVC 的 apps subPath（部署侧挂到 /app/app-workspace）
                ServiceType::UserApp => "/app/app-workspace".to_string(),
                // UserAppBuilder 完整开发容器: per-app RWO PVC 整卷挂载（与镜像
                // start-up.sh 导出的 USERAPP_WORKSPACE_DIR 一致）
                ServiceType::UserAppBuilder => crate::paths::USERAPP_WORKSPACE_ROOT.to_string(),
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
    pub fn resolve_container_path(&self, variables: &HashMap<String, String>) -> String {
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
