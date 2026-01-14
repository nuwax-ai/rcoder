//! 路径解析器
//!
//! 从 rcoder/utils/host_path_resolver.rs 迁移

use crate::{ContainerSelfInspector, DockerError, DockerResult};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// 宿主机路径解析器
///
/// 用于将容器内的路径解析为宿主机路径
/// 支持容器嵌套场景（容器内运行容器）
#[derive(Clone)]
pub struct HostPathResolver {
    /// 宿主机的项目工作空间根目录（默认工作空间，用于相对路径解析）
    host_project_workspace: PathBuf,
    /// 容器内的项目工作空间根目录（默认工作空间，用于相对路径解析）
    container_project_workspace: PathBuf,
    /// 所有挂载点缓存，按路径长度降序排列（最具体的路径优先匹配）
    /// 格式: Vec<(container_path, host_path)>
    all_mounts: Vec<(PathBuf, PathBuf)>,
    /// 容器自检器（用于自动检测路径映射）
    inspector: Option<Arc<ContainerSelfInspector>>,
}

impl HostPathResolver {
    /// 创建新的路径解析器（自动检测配置）
    ///
    /// 使用默认 Docker socket 路径自动检测路径映射
    ///
    /// # Returns
    /// * `DockerResult<Self>` - 路径解析器或错误
    pub async fn new() -> DockerResult<Self> {
        Self::new_with_docker_socket(None).await
    }

    /// 使用指定的 Docker socket 创建路径解析器
    ///
    /// # Arguments
    /// * `docker_socket_path` - Docker socket 路径（None 表示使用默认路径）
    ///
    /// # Returns
    /// * `DockerResult<Self>` - 路径解析器或错误
    pub async fn new_with_docker_socket(docker_socket_path: Option<String>) -> DockerResult<Self> {
        debug!("开始创建 HostPathResolver");

        // 创建容器自检器
        let socket_path = docker_socket_path
            .as_deref()
            .unwrap_or("/var/run/docker.sock");

        let inspector = Arc::new(
            ContainerSelfInspector::new(socket_path)
                .await
                .map_err(|e| {
                    DockerError::ConfigurationError(format!("创建容器自检器失败: {}", e))
                })?,
        );

        // 获取所有挂载点信息
        let mounts = inspector
            .get_all_mounts()
            .await
            .map_err(|e| DockerError::ConfigurationError(format!("获取挂载点失败: {}", e)))?;

        if mounts.is_empty() {
            return Err(DockerError::ConfigurationError(
                "未检测到任何挂载点，请检查是否在容器内运行，并确保 docker-compose.yml 中配置了 volume 挂载".to_string()
            ));
        }

        // 🔍 缓存所有挂载点，按路径长度降序排列（最具体的路径优先匹配）
        let mut all_mounts: Vec<(PathBuf, PathBuf)> = mounts
            .iter()
            .map(|(cp, hp)| (PathBuf::from(cp), PathBuf::from(hp)))
            .collect();
        // 按容器路径长度降序排列，确保最具体的路径优先匹配
        all_mounts.sort_by(|a, b| b.0.as_os_str().len().cmp(&a.0.as_os_str().len()));

        info!(
            "📁 [HostPathResolver] 缓存 {} 个挂载点（按路径长度降序）:",
            all_mounts.len()
        );
        for (idx, (cp, hp)) in all_mounts.iter().enumerate() {
            debug!("  {}. {} -> {}", idx + 1, cp.display(), hp.display());
        }

        // 选择默认工作空间（用于相对路径解析）
        // 优先选择 project_workspace 相关的挂载点
        let default_workspace = all_mounts
            .iter()
            .find(|(container_path, _)| {
                let path_str = container_path.to_string_lossy();
                path_str.contains("computer-project-workspace")
                    || path_str.contains("project_workspace")
            })
            .or_else(|| all_mounts.first())
            // all_mounts 不为空（前面已检查），所以这里一定有值
            .expect("all_mounts is not empty, checked above");

        let (container_workspace, host_workspace) = default_workspace.clone();

        info!(
            "✅ [HostPathResolver] 默认工作空间: {} (host) -> {} (container)",
            host_workspace.display(),
            container_workspace.display()
        );

        Ok(Self {
            host_project_workspace: host_workspace,
            container_project_workspace: container_workspace,
            all_mounts,
            inspector: Some(inspector),
        })
    }

    /// 将容器内路径解析为宿主机路径
    ///
    /// # Arguments
    /// * `container_path` - 容器内的路径
    ///
    /// # Returns
    /// * `DockerResult<PathBuf>` - 宿主机路径或错误
    ///
    /// # Examples
    /// ```no_run
    /// use docker_manager::path::HostPathResolver;
    /// use std::path::Path;
    ///
    /// # async fn example() -> docker_manager::DockerResult<()> {
    /// let resolver = HostPathResolver::new().await?;
    /// let host_path = resolver.resolve_to_host_path(
    ///     Path::new("/app/project_workspace/project-123/src")
    /// )?;
    /// println!("Host path: {}", host_path.display());
    /// # Ok(())
    /// # }
    /// ```
    pub fn resolve_to_host_path(&self, container_path: &Path) -> DockerResult<PathBuf> {
        debug!("解析容器路径到宿主机路径: {}", container_path.display());

        // 如果路径是相对路径，先转换为绝对路径（相对于容器工作空间）
        let container_path = if container_path.is_relative() {
            self.container_project_workspace.join(container_path)
        } else {
            container_path.to_path_buf()
        };

        // 🆕 从缓存的挂载点中查找最具体的匹配
        // all_mounts 已按路径长度降序排列，所以第一个匹配就是最具体的
        for (mount_container, mount_host) in &self.all_mounts {
            if container_path.starts_with(mount_container) {
                // 找到匹配的挂载点
                let relative_path = container_path
                    .strip_prefix(mount_container)
                    .unwrap_or(Path::new(""));
                let host_path = mount_host.join(relative_path);

                debug!(
                    "✅ 从挂载点解析: {} -> {} (mount: {} -> {})",
                    container_path.display(),
                    host_path.display(),
                    mount_container.display(),
                    mount_host.display()
                );

                return Ok(host_path);
            }
        }

        // 没有找到匹配的挂载点，返回错误
        warn!(
            "⚠️ 无法解析路径 {}: 未找到匹配的挂载点",
            container_path.display()
        );
        Err(DockerError::ConfigurationError(format!(
            "无法解析容器路径 '{}': 未找到匹配的挂载点。可用挂载点: {:?}",
            container_path.display(),
            self.all_mounts
                .iter()
                .map(|(c, h)| format!("{} -> {}", c.display(), h.display()))
                .collect::<Vec<_>>()
        )))
    }

    /// 获取宿主机工作空间根目录
    pub fn host_workspace_base(&self) -> &Path {
        &self.host_project_workspace
    }

    /// 获取容器工作空间根目录
    pub fn container_workspace_base(&self) -> &Path {
        &self.container_project_workspace
    }

    /// 验证路径解析器配置是否有效
    ///
    /// # Returns
    /// * `DockerResult<()>` - 验证成功或错误
    pub fn validate(&self) -> DockerResult<()> {
        if !self.host_project_workspace.is_absolute() {
            return Err(DockerError::ConfigurationError(
                "宿主机工作空间必须是绝对路径".to_string(),
            ));
        }

        if !self.container_project_workspace.is_absolute() {
            return Err(DockerError::ConfigurationError(
                "容器工作空间必须是绝对路径".to_string(),
            ));
        }

        Ok(())
    }

    /// 获取诊断信息（用于调试）
    pub fn get_diagnostics(&self) -> String {
        format!(
            "HostPathResolver {{\n  \
             host_workspace: {},\n  \
             container_workspace: {},\n  \
             all_mounts: {:?},\n  \
             inspector: {}\n\
             }}",
            self.host_project_workspace.display(),
            self.container_project_workspace.display(),
            self.all_mounts
                .iter()
                .map(|(c, h)| format!("{} -> {}", c.display(), h.display()))
                .collect::<Vec<_>>(),
            if self.inspector.is_some() {
                "enabled"
            } else {
                "disabled"
            }
        )
    }

    /// 检查 Docker 连接状态
    ///
    /// # Returns
    /// * `DockerResult<bool>` - 是否可以连接到 Docker
    pub async fn check_docker_connection(&self) -> DockerResult<bool> {
        if let Some(inspector) = &self.inspector {
            inspector
                .verify_docker_connection()
                .await
                .map(|_| true)
                .map_err(|e| DockerError::ConnectionError(format!("Docker 连接检查失败: {}", e)))
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_resolver() -> HostPathResolver {
        HostPathResolver {
            host_project_workspace: PathBuf::from("/host/projects"),
            container_project_workspace: PathBuf::from("/app/project_workspace"),
            all_mounts: vec![
                // 按路径长度降序排列
                (
                    PathBuf::from("/app/project_workspace"),
                    PathBuf::from("/host/projects"),
                ),
                (PathBuf::from("/app/logs"), PathBuf::from("/host/logs")),
                (PathBuf::from("/app"), PathBuf::from("/host/app")),
            ],
            inspector: None,
        }
    }

    #[test]
    fn test_resolve_relative_path() {
        let resolver = create_test_resolver();

        let container_path = Path::new("project-123/src/main.rs");
        let host_path = resolver.resolve_to_host_path(container_path).unwrap();

        assert_eq!(
            host_path,
            PathBuf::from("/host/projects/project-123/src/main.rs")
        );
    }

    #[test]
    fn test_resolve_absolute_path() {
        let resolver = create_test_resolver();

        let container_path = Path::new("/app/project_workspace/project-123/src/main.rs");
        let host_path = resolver.resolve_to_host_path(container_path).unwrap();

        assert_eq!(
            host_path,
            PathBuf::from("/host/projects/project-123/src/main.rs")
        );
    }

    #[test]
    fn test_resolve_logs_path() {
        // 🆕 测试 /app/logs 路径解析（这是本次修复的关键测试）
        let resolver = create_test_resolver();

        let container_path = Path::new("/app/logs/container");
        let host_path = resolver.resolve_to_host_path(container_path).unwrap();

        assert_eq!(host_path, PathBuf::from("/host/logs/container"));
    }

    #[test]
    fn test_resolve_logs_with_subdir() {
        // 🆕 测试 /app/logs/container/subdir 路径解析
        let resolver = create_test_resolver();

        let container_path = Path::new("/app/logs/container/agent-runner-xxx-20251226");
        let host_path = resolver.resolve_to_host_path(container_path).unwrap();

        assert_eq!(
            host_path,
            PathBuf::from("/host/logs/container/agent-runner-xxx-20251226")
        );
    }

    #[test]
    fn test_resolve_outside_all_mounts() {
        let resolver = create_test_resolver();

        let container_path = Path::new("/etc/passwd");
        let result = resolver.resolve_to_host_path(container_path);

        // 不在任何挂载点内，应该返回错误
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_success() {
        let resolver = create_test_resolver();
        assert!(resolver.validate().is_ok());
    }

    #[test]
    fn test_validate_relative_host_path() {
        let resolver = HostPathResolver {
            host_project_workspace: PathBuf::from("host/projects"), // 相对路径
            container_project_workspace: PathBuf::from("/app/project_workspace"),
            all_mounts: vec![(
                PathBuf::from("/app/project_workspace"),
                PathBuf::from("host/projects"),
            )],
            inspector: None,
        };

        assert!(resolver.validate().is_err());
    }

    #[test]
    fn test_workspace_base_accessors() {
        let resolver = create_test_resolver();

        assert_eq!(resolver.host_workspace_base(), Path::new("/host/projects"));
        assert_eq!(
            resolver.container_workspace_base(),
            Path::new("/app/project_workspace")
        );
    }

    #[test]
    fn test_mount_order_priority() {
        // 🆕 测试挂载点优先级：更具体的路径应该优先匹配
        let resolver = HostPathResolver {
            host_project_workspace: PathBuf::from("/host/projects"),
            container_project_workspace: PathBuf::from("/app/project_workspace"),
            all_mounts: vec![
                // 最长路径优先
                (
                    PathBuf::from("/app/project_workspace/special"),
                    PathBuf::from("/host/special"),
                ),
                (
                    PathBuf::from("/app/project_workspace"),
                    PathBuf::from("/host/projects"),
                ),
                (PathBuf::from("/app"), PathBuf::from("/host/app")),
            ],
            inspector: None,
        };

        // /app/project_workspace/special/file 应该匹配第一个挂载点
        let path1 = Path::new("/app/project_workspace/special/file.txt");
        let result1 = resolver.resolve_to_host_path(path1).unwrap();
        assert_eq!(result1, PathBuf::from("/host/special/file.txt"));

        // /app/project_workspace/other/file 应该匹配第二个挂载点
        let path2 = Path::new("/app/project_workspace/other/file.txt");
        let result2 = resolver.resolve_to_host_path(path2).unwrap();
        assert_eq!(result2, PathBuf::from("/host/projects/other/file.txt"));
    }

    // 注意：以下测试需要在 Docker 容器环境中运行
    #[tokio::test]
    #[ignore]
    async fn test_new_in_container_environment() {
        // 此测试仅在容器内有效
        let result = HostPathResolver::new().await;
        if std::env::var("HOSTNAME").is_ok() {
            assert!(result.is_ok());
        }
    }
}
