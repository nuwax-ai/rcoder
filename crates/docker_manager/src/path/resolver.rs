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
    /// 宿主机的项目工作空间根目录
    host_project_workspace: PathBuf,
    /// 容器内的项目工作空间根目录
    container_project_workspace: PathBuf,
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

        // 获取第一个有效的挂载点信息
        let mounts = inspector
            .get_all_mounts()
            .await
            .map_err(|e| DockerError::ConfigurationError(format!("获取挂载点失败: {}", e)))?;

        if mounts.is_empty() {
            warn!("未检测到任何挂载点，将使用容器内路径作为宿主机路径");
            return Ok(Self {
                host_project_workspace: PathBuf::from("/app"),
                container_project_workspace: PathBuf::from("/app"),
                inspector: Some(inspector),
            });
        }

        // 🔍 改进的挂载点匹配逻辑，避免模糊匹配导致的错误
        // mounts 格式: Vec<(container_path, host_path)>
        debug!(
            "🔍 路径解析调试: 可用挂载点={:?}",
            mounts
                .iter()
                .map(|(cp, hs)| format!("{}→{}", cp, hs))
                .collect::<Vec<_>>()
        );

        // 优先查找最具体的匹配：computer-project-workspace > project_workspace
        let mount_info = mounts
            .iter()
            .find(|(container_path, _)| {
                // 优先匹配 computer-project-workspace
                container_path.contains("computer-project-workspace")
            })
            .or_else(|| {
                // 回退：匹配 project_workspace
                debug!("🔍 未找到 computer-project-workspace 匹配，尝试 project_workspace...");
                mounts
                    .iter()
                    .find(|(container_path, _)| container_path.contains("project_workspace"))
            });

        // 🚨 关键修复：不回退到第一个挂载点，而是报错！
        let mount_info = mount_info.ok_or_else(|| {
            let available_mounts = mounts
                .iter()
                .map(|(cp, hs)| format!("{} -> {}", cp, hs))
                .collect::<Vec<_>>()
                .join(", ");

            DockerError::ConfigurationError(format!(
                "无法找到匹配的挂载点信息。可用的挂载点: {}. 请检查 docker-compose.yml 中的挂载配置。",
                available_mounts
            ))
        })?;

        let (container_workspace, host_workspace) = mount_info;
        debug!(
            "✅ 路径解析结果: {} (container) -> {} (host)",
            container_workspace, host_workspace
        );
        let host_workspace = PathBuf::from(host_workspace);
        let container_workspace = PathBuf::from(container_workspace);

        info!(
            "✅ 检测到路径映射: {} (host) -> {} (container)",
            host_workspace.display(),
            container_workspace.display()
        );

        Ok(Self {
            host_project_workspace: host_workspace,
            container_project_workspace: container_workspace,
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

        // 检查路径是否在容器工作空间内
        if container_path.starts_with(&self.container_project_workspace) {
            // 计算相对路径
            let relative_path = container_path
                .strip_prefix(&self.container_project_workspace)
                .map_err(|e| DockerError::ConfigurationError(format!("路径解析失败: {}", e)))?;

            // 拼接到宿主机工作空间
            let host_path = self.host_project_workspace.join(relative_path);

            debug!(
                "✅ 解析结果: {} (container) -> {} (host)",
                container_path.display(),
                host_path.display()
            );

            return Ok(host_path);
        }

        // 路径不在工作空间内，尝试从所有挂载点中查找匹配
        debug!(
            "容器路径 {} 不在工作空间 {} 内，尝试从挂载点查找",
            container_path.display(),
            self.container_project_workspace.display()
        );

        // 使用 inspector 获取所有挂载点并查找匹配
        if let Some(inspector) = &self.inspector {
            // 注意：这里需要阻塞调用，因为 resolve_to_host_path 不是 async
            // 改为使用缓存的挂载点信息
            // TODO: 后续可以改为在创建时缓存所有挂载点
            if let Ok(mounts) = std::thread::scope(|s| {
                s.spawn(|| {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to create tokio runtime for mount resolution");
                    rt.block_on(async { inspector.get_all_mounts().await })
                })
                .join()
                .expect("mount resolution thread panicked")
            }) {
                // 查找匹配的挂载点
                for (mount_container_path, mount_host_path) in &mounts {
                    let mount_container = Path::new(mount_container_path);
                    if container_path.starts_with(mount_container) {
                        // 找到匹配的挂载点
                        let relative_path = container_path
                            .strip_prefix(mount_container)
                            .unwrap_or(Path::new(""));
                        let host_path = PathBuf::from(mount_host_path).join(relative_path);

                        info!(
                            "✅ 从挂载点解析: {} -> {} (mount: {} -> {})",
                            container_path.display(),
                            host_path.display(),
                            mount_container_path,
                            mount_host_path
                        );

                        return Ok(host_path);
                    }
                }
            }
        }

        // 没有找到匹配的挂载点，返回错误
        warn!(
            "⚠️ 无法解析路径 {}: 不在工作空间内且未找到匹配的挂载点",
            container_path.display()
        );
        Err(DockerError::ConfigurationError(format!(
            "无法解析容器路径 '{}': 不在工作空间内且未找到匹配的挂载点",
            container_path.display()
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
             inspector: {}\n\
             }}",
            self.host_project_workspace.display(),
            self.container_project_workspace.display(),
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

    #[test]
    fn test_resolve_relative_path() {
        let resolver = HostPathResolver {
            host_project_workspace: PathBuf::from("/host/projects"),
            container_project_workspace: PathBuf::from("/app/project_workspace"),
            inspector: None,
        };

        let container_path = Path::new("project-123/src/main.rs");
        let host_path = resolver.resolve_to_host_path(container_path).unwrap();

        assert_eq!(
            host_path,
            PathBuf::from("/host/projects/project-123/src/main.rs")
        );
    }

    #[test]
    fn test_resolve_absolute_path() {
        let resolver = HostPathResolver {
            host_project_workspace: PathBuf::from("/host/projects"),
            container_project_workspace: PathBuf::from("/app/project_workspace"),
            inspector: None,
        };

        let container_path = Path::new("/app/project_workspace/project-123/src/main.rs");
        let host_path = resolver.resolve_to_host_path(container_path).unwrap();

        assert_eq!(
            host_path,
            PathBuf::from("/host/projects/project-123/src/main.rs")
        );
    }

    #[test]
    fn test_resolve_outside_workspace() {
        let resolver = HostPathResolver {
            host_project_workspace: PathBuf::from("/host/projects"),
            container_project_workspace: PathBuf::from("/app/project_workspace"),
            inspector: None,
        };

        let container_path = Path::new("/etc/passwd");
        let result = resolver.resolve_to_host_path(container_path);

        // 不在工作空间内且没有 inspector，应该返回错误
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_success() {
        let resolver = HostPathResolver {
            host_project_workspace: PathBuf::from("/host/projects"),
            container_project_workspace: PathBuf::from("/app/project_workspace"),
            inspector: None,
        };

        assert!(resolver.validate().is_ok());
    }

    #[test]
    fn test_validate_relative_host_path() {
        let resolver = HostPathResolver {
            host_project_workspace: PathBuf::from("host/projects"), // 相对路径
            container_project_workspace: PathBuf::from("/app/project_workspace"),
            inspector: None,
        };

        assert!(resolver.validate().is_err());
    }

    #[test]
    fn test_workspace_base_accessors() {
        let resolver = HostPathResolver {
            host_project_workspace: PathBuf::from("/host/projects"),
            container_project_workspace: PathBuf::from("/app/project_workspace"),
            inspector: None,
        };

        assert_eq!(resolver.host_workspace_base(), Path::new("/host/projects"));
        assert_eq!(
            resolver.container_workspace_base(),
            Path::new("/app/project_workspace")
        );
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
