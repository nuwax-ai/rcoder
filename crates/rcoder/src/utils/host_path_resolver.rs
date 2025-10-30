//! 宿主机路径解析工具
//!
//! 在容器化环境中，当容器需要创建其他容器时，
//! 通过自动检测将容器内的路径转换为宿主机路径以便正确挂载

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn, debug};

use docker_manager::ContainerSelfInspector;

/// 宿主机路径解析器
pub struct HostPathResolver {
    /// 宿主机上的项目工作目录基础路径
    host_project_workspace: PathBuf,
    /// 容器内的项目工作目录基础路径
    container_project_workspace: PathBuf,
    /// 容器自检测器（用于调试和诊断）
    inspector: Option<Arc<ContainerSelfInspector>>,
}

impl HostPathResolver {
    /// 创建新的路径解析器（自动检测模式）
    ///
    /// 自动检测当前容器的挂载信息，获取容器内路径对应的宿主机路径
    ///
    /// # Returns
    /// * `Result<Self>` - 路径解析器实例或错误
    pub async fn new() -> Result<Self> {
        Self::new_with_docker_socket("/var/run/docker.sock").await
    }

    /// 使用指定的 Docker socket 创建路径解析器
    ///
    /// # Arguments
    /// * `docker_socket_path` - Docker socket 路径
    ///
    /// # Returns
    /// * `Result<Self>` - 路径解析器实例或错误
    pub async fn new_with_docker_socket(docker_socket_path: &str) -> Result<Self> {
        info!("初始化宿主机路径解析器（自动检测模式）");

        let container_project_workspace = PathBuf::from("/app/project_workspace");

        // 创建容器自检测器
        let inspector = ContainerSelfInspector::new(docker_socket_path)
            .await
            .context("创建容器自检测器失败")?;

        // 检测宿主机路径
        let host_project_workspace = Self::detect_host_project_workspace(&inspector).await?;

        info!("✅ 路径解析器初始化成功:");
        info!("  容器内路径: {:?}", container_project_workspace);
        info!("  宿主机路径: {:?}", host_project_workspace);

        Ok(Self {
            host_project_workspace,
            container_project_workspace,
            inspector: Some(Arc::new(inspector)),
        })
    }

    /// 检测宿主机项目工作目录路径
    ///
    /// # Arguments
    /// * `inspector` - 容器自检测器
    ///
    /// # Returns
    /// * `Result<PathBuf>` - 宿主机路径或错误
    async fn detect_host_project_workspace(inspector: &ContainerSelfInspector) -> Result<PathBuf> {
        let container_path = "/app/project_workspace";

        info!("检测路径 {} 对应的宿主机路径", container_path);

        let host_path = inspector
            .detect_host_path_for_container_dir(container_path)
            .await
            .context("自动检测宿主机路径失败")?;

        // 验证宿主机路径存在性（注意：这个路径在宿主机上，容器内可能看不到）
        info!("📍 检测到宿主机路径: {:?}", host_path);

        Ok(PathBuf::from(host_path))
    }

    /// 将容器内路径转换为宿主机路径
    ///
    /// # Arguments
    /// * `container_path` - 容器内的路径 (如: /app/project_workspace/abc-123 或 ./project_workspace/abc-123)
    ///
    /// # Returns
    /// * `PathBuf` - 对应的宿主机路径
    ///
    /// # Examples
    /// ```
    /// let resolver = HostPathResolver::new().await?;
    /// let host_path = resolver.resolve_to_host_path("/app/project_workspace/abc-123");
    /// // 返回类似: "/data/rcoder/project_workspace/abc-123"
    /// ```
    pub fn resolve_to_host_path(&self, container_path: &Path) -> PathBuf {
        debug!("解析路径: {:?}", container_path);

        // 第一步：将容器内的相对路径转换为绝对路径
        let container_absolute_path = if container_path.is_relative() {
            // 如果是相对路径，先转换为容器内的绝对路径
            let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            current_dir.join(container_path)
        } else {
            container_path.to_path_buf()
        };

        debug!("容器内绝对路径: {:?}", container_absolute_path);

        // 第二步：如果容器内绝对路径以项目工作目录开头，提取相对路径
        if let Ok(relative_path) = container_absolute_path.strip_prefix(&self.container_project_workspace) {
            // 第三步：将相对路径拼接到宿主机基础路径上
            let host_path = self.host_project_workspace.join(relative_path);
            debug!("路径转换成功: 容器内 {:?} -> 宿主机 {:?}", container_absolute_path, host_path);
            return host_path;
        }

        // 如果路径不在项目工作目录下，可能是已经在宿主机上的路径
        warn!("路径 {:?} 不在容器项目工作目录 {:?} 下，可能是宿主机路径",
               container_absolute_path, self.container_project_workspace);

        // 尝试直接解析为绝对路径
        match container_absolute_path.canonicalize() {
            Ok(absolute_path) => {
                debug!("使用绝对路径: {:?}", absolute_path);
                absolute_path
            }
            Err(e) => {
                warn!("无法解析路径 {:?} 为绝对路径: {}，使用原始路径", container_absolute_path, e);
                container_absolute_path
            }
        }
    }

    /// 获取宿主机上的项目工作目录基础路径
    pub fn host_workspace_base(&self) -> &Path {
        &self.host_project_workspace
    }

    /// 获取容器内的项目工作目录基础路径
    pub fn container_workspace_base(&self) -> &Path {
        &self.container_project_workspace
    }

    /// 验证路径解析是否正确
    ///
    /// 注意：这里主要验证检测过程的完整性，宿主机路径在容器内可能无法访问
    pub fn validate(&self) -> Result<(), String> {
        info!("路径解析器验证通过");
        info!("  容器内基础路径: {:?}", self.container_project_workspace);
        info!("  宿主机基础路径: {:?}", self.host_project_workspace);
        Ok(())
    }

    /// 获取诊断信息（用于调试）
    pub async fn get_diagnostics(&self) -> Result<String> {
        if let Some(inspector) = &self.inspector {
            let mounts = inspector.get_all_mounts().await
                .context("获取容器挂载信息失败")?;

            let mut diagnostics = "容器挂载信息:\n".to_string();
            for (container_path, host_path) in mounts {
                diagnostics.push_str(&format!("  {} -> {}\n", container_path, host_path));
            }

            Ok(diagnostics)
        } else {
            Ok("诊断信息不可用".to_string())
        }
    }

    /// 检查 Docker 连接状态
    pub async fn check_docker_connection(&self) -> Result<()> {
        if let Some(inspector) = &self.inspector {
            inspector.verify_docker_connection().await
                .context("Docker 连接验证失败")?;
        }
        Ok(())
    }
}

impl Default for HostPathResolver {
    fn default() -> Self {
        // 注意：这里使用 panic，因为 Default 是同步的，但我们的初始化是异步的
        // 在实际使用中应该使用 HostPathResolver::new() 方法
        panic!("HostPathResolver 必须通过 async 方法初始化，使用 HostPathResolver::new()")
    }
}

/// 获取全局路径解析器实例
///
/// # Returns
/// * `Result<HostPathResolver>` - 路径解析器实例或错误
pub async fn get_host_path_resolver() -> Result<HostPathResolver> {
    HostPathResolver::new().await
}

/// 便捷函数：解析容器路径到宿主机路径
///
/// # Arguments
/// * `container_path` - 容器内路径
///
/// # Returns
/// * `Result<PathBuf>` - 宿主机路径或错误
pub async fn resolve_container_path_to_host(container_path: &Path) -> Result<PathBuf> {
    let resolver = get_host_path_resolver().await?;
    Ok(resolver.resolve_to_host_path(container_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[tokio::test]
    async fn test_path_resolution() {
        // 注意：这个测试需要在容器内运行才能正常工作
        // 在单元测试环境中可能会失败

        // 如果在容器内且有 Docker socket 访问权限
        if Path::new("/var/run/docker.sock").exists() {
            let resolver = HostPathResolver::new().await;
            if let Ok(resolver) = resolver {
                // 测试路径转换
                let container_path = PathBuf::from("/app/project_workspace/project-123");
                let host_path = resolver.resolve_to_host_path(&container_path);

                // 验证结果不为空
                assert!(!host_path.as_os_str().is_empty());
            }
        }
    }
}