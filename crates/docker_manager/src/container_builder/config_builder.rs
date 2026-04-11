//! 容器配置构建器
//!
//! 使用 Builder 模式构建 DockerContainerConfig

use crate::{DockerContainerConfig, DockerResult, MountPoint, ResourceLimits};
use std::collections::HashMap;
use tracing::debug;

/// 容器配置构建器
///
/// 使用 Builder 模式提供灵活的容器配置构建接口
///
/// # Examples
/// ```no_run
/// use docker_manager::container_builder::ContainerConfigBuilder;
///
/// # async fn example() -> docker_manager::DockerResult<()> {
/// let config = ContainerConfigBuilder::new("project-123")
///     .image("registry.example.com/agent-runner:latest")
///     .host_path("/host/path")
///     .container_path("/app/project_workspace/project-123")
///     .env("PROJECT_ID", "project-123")
///     .network_name("rcoder_agent-network")
///     .build()?;
/// # Ok(())
/// # }
/// ```
pub struct ContainerConfigBuilder {
    project_id: String,
    image: Option<String>,
    name_prefix: Option<String>,
    host_path: Option<String>,
    container_path: Option<String>,
    work_dir: Option<String>,
    env_vars: HashMap<String, String>,
    port_bindings: HashMap<String, String>,
    network_mode: Option<String>,
    auto_remove: bool,
    resource_limits: Option<ResourceLimits>,
    extra_mounts: Vec<MountPoint>,
    command: Option<Vec<String>>,
    entrypoint: Option<Vec<String>>,
    network_name: Option<String>,
}

impl ContainerConfigBuilder {
    /// 创建新的容器配置构建器
    ///
    /// # Arguments
    /// * `project_id` - 项目ID（必需）
    pub fn new(project_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            image: None,
            name_prefix: None,
            host_path: None,
            container_path: None,
            work_dir: None,
            env_vars: HashMap::new(),
            port_bindings: HashMap::new(),
            network_mode: None,
            auto_remove: false,
            resource_limits: None,
            extra_mounts: Vec::new(),
            command: None,
            entrypoint: None,
            network_name: None,
        }
    }

    /// 设置 Docker 镜像
    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.image = Some(image.into());
        self
    }

    /// 设置容器名称前缀
    pub fn name_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.name_prefix = Some(prefix.into());
        self
    }

    /// 设置宿主机路径
    pub fn host_path(mut self, path: impl Into<String>) -> Self {
        self.host_path = Some(path.into());
        self
    }

    /// 设置容器内路径
    pub fn container_path(mut self, path: impl Into<String>) -> Self {
        self.container_path = Some(path.into());
        self
    }

    /// 设置工作目录
    pub fn work_dir(mut self, dir: impl Into<String>) -> Self {
        self.work_dir = Some(dir.into());
        self
    }

    /// 添加单个环境变量
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env_vars.insert(key.into(), value.into());
        self
    }

    /// 批量添加环境变量
    pub fn envs(mut self, vars: HashMap<String, String>) -> Self {
        self.env_vars.extend(vars);
        self
    }

    /// 添加端口映射
    pub fn port_binding(
        mut self,
        container_port: impl Into<String>,
        host_port: impl Into<String>,
    ) -> Self {
        self.port_bindings
            .insert(container_port.into(), host_port.into());
        self
    }

    /// 批量添加端口映射
    pub fn port_bindings(mut self, bindings: HashMap<String, String>) -> Self {
        self.port_bindings.extend(bindings);
        self
    }

    /// 设置网络模式
    pub fn network_mode(mut self, mode: impl Into<String>) -> Self {
        self.network_mode = Some(mode.into());
        self
    }

    /// 设置自动删除标志
    pub fn auto_remove(mut self, enabled: bool) -> Self {
        self.auto_remove = enabled;
        self
    }

    /// 设置资源限制
    pub fn resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.resource_limits = Some(limits);
        self
    }

    /// 添加单个挂载点
    pub fn add_mount(mut self, mount: MountPoint) -> Self {
        self.extra_mounts.push(mount);
        self
    }

    /// 批量添加挂载点
    pub fn add_mounts(mut self, mounts: Vec<MountPoint>) -> Self {
        self.extra_mounts.extend(mounts);
        self
    }

    /// 设置启动命令
    pub fn command(mut self, command: Vec<String>) -> Self {
        self.command = Some(command);
        self
    }

    /// 设置入口点
    pub fn entrypoint(mut self, entrypoint: Vec<String>) -> Self {
        self.entrypoint = Some(entrypoint);
        self
    }

    /// 设置网络名称
    pub fn network_name(mut self, name: impl Into<String>) -> Self {
        self.network_name = Some(name.into());
        self
    }

    /// 构建 DockerContainerConfig
    ///
    /// # Returns
    /// * `DockerResult<DockerContainerConfig>` - 构建的配置或错误
    pub fn build(self) -> DockerResult<DockerContainerConfig> {
        debug!("builtcontainerconfig, projectID: {}", self.project_id);

        // 使用默认值或提供的值
        let image = self.image.unwrap_or_else(crate::default_docker_image);
        let name_prefix = self
            .name_prefix
            .unwrap_or_else(|| "rcoder-agent".to_string());
        let host_path = self.host_path.unwrap_or_default();
        let container_path = self
            .container_path
            .unwrap_or_else(|| crate::DEFAULT_WORK_DIR.to_string());
        let work_dir = self
            .work_dir
            .unwrap_or_else(|| crate::DEFAULT_WORK_DIR.to_string());
        let network_mode = self
            .network_mode
            .unwrap_or_else(|| crate::DEFAULT_NETWORK_MODE.to_string());

        let config = DockerContainerConfig {
            project_id: self.project_id,
            image,
            name_prefix,
            host_path,
            container_path,
            work_dir,
            env_vars: self.env_vars,
            port_bindings: self.port_bindings,
            network_mode,
            auto_remove: self.auto_remove,
            resource_limits: self.resource_limits,
            extra_mounts: self.extra_mounts,
            command: self.command,
            entrypoint: self.entrypoint,
            network_name: self.network_name,
        };

        debug!(
            "Container config built: image={}, network={:?}, mounts={}",
            config.image,
            config.network_name,
            config.extra_mounts.len()
        );

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_minimal() {
        let config = ContainerConfigBuilder::new("test-project").build().unwrap();

        assert_eq!(config.project_id, "test-project");
        assert_eq!(config.name_prefix, "rcoder-agent");
        assert!(!config.auto_remove);
    }

    #[test]
    fn test_builder_full() {
        let config = ContainerConfigBuilder::new("test-project")
            .image("custom-image:latest")
            .name_prefix("custom-prefix")
            .host_path("/host/path")
            .container_path("/container/path")
            .work_dir("/work")
            .env("KEY1", "value1")
            .env("KEY2", "value2")
            .port_binding("8080", "8080")
            .network_mode("bridge")
            .auto_remove(true)
            .network_name("test-network")
            .build()
            .unwrap();

        assert_eq!(config.project_id, "test-project");
        assert_eq!(config.image, "custom-image:latest");
        assert_eq!(config.name_prefix, "custom-prefix");
        assert_eq!(config.host_path, "/host/path");
        assert_eq!(config.container_path, "/container/path");
        assert_eq!(config.work_dir, "/work");
        assert_eq!(config.env_vars.len(), 2);
        assert_eq!(config.port_bindings.len(), 1);
        assert_eq!(config.network_mode, "bridge");
        assert!(config.auto_remove);
        assert_eq!(config.network_name, Some("test-network".to_string()));
    }

    #[test]
    fn test_builder_with_mounts() {
        let mount = MountPoint {
            host_path: "/host/mount".to_string(),
            container_path: "/container/mount".to_string(),
            read_only: false,
        };

        let config = ContainerConfigBuilder::new("test-project")
            .add_mount(mount)
            .build()
            .unwrap();

        assert_eq!(config.extra_mounts.len(), 1);
        assert_eq!(config.extra_mounts[0].host_path, "/host/mount");
    }

    #[test]
    fn test_builder_with_resource_limits() {
        let limits = ResourceLimits {
            memory_limit: Some((512 * 1024 * 1024) as f64), // 512MB
            cpu_limit: Some(1.0),
            swap_limit: None,
        };

        let config = ContainerConfigBuilder::new("test-project")
            .resource_limits(limits)
            .build()
            .unwrap();

        assert!(config.resource_limits.is_some());
        let resource_limits = config.resource_limits.unwrap();
        assert_eq!(
            resource_limits.memory_limit,
            Some((512 * 1024 * 1024) as f64)
        );
        assert_eq!(resource_limits.cpu_limit, Some(1.0));
    }
}
