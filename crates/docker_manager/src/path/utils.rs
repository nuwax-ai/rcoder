//! 路径解析工具函数
//!
//! 提供便捷的路径解析接口

use crate::path::HostPathResolver;
use crate::DockerResult;
use std::path::{Path, PathBuf};

/// 便捷函数：将容器路径解析为宿主机路径
///
/// 自动创建 `HostPathResolver` 并执行路径解析
///
/// # Arguments
/// * `container_path` - 容器内的路径
///
/// # Returns
/// * `DockerResult<PathBuf>` - 宿主机路径或错误
///
/// # Examples
/// ```no_run
/// use docker_manager::path::resolve_container_path_to_host;
/// use std::path::Path;
///
/// # async fn example() -> docker_manager::DockerResult<()> {
/// let host_path = resolve_container_path_to_host(
///     Path::new("/app/project_workspace/project-123")
/// ).await?;
/// println!("Host path: {}", host_path.display());
/// # Ok(())
/// # }
/// ```
pub async fn resolve_container_path_to_host(container_path: &Path) -> DockerResult<PathBuf> {
    let resolver = HostPathResolver::new().await?;
    resolver.resolve_to_host_path(container_path)
}

/// 便捷函数：获取 HostPathResolver 实例
///
/// 使用默认配置创建路径解析器
///
/// # Returns
/// * `DockerResult<HostPathResolver>` - 路径解析器或错误
pub async fn get_host_path_resolver() -> DockerResult<HostPathResolver> {
    HostPathResolver::new().await
}

/// 标准化路径（移除冗余的 `.` 和 `..` 组件）
///
/// # Arguments
/// * `path` - 要标准化的路径
///
/// # Returns
/// * `PathBuf` - 标准化后的路径
///
/// # Examples
/// ```
/// use docker_manager::path::normalize_path;
/// use std::path::Path;
///
/// let normalized = normalize_path(Path::new("/app/./project/../project/src"));
/// assert_eq!(normalized, Path::new("/app/project/src"));
/// ```
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::CurDir => {
                // 跳过 `.`
            }
            std::path::Component::ParentDir => {
                // 处理 `..`
                if !components.is_empty() {
                    components.pop();
                }
            }
            other => {
                components.push(other);
            }
        }
    }

    components.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_path_current_dir() {
        let path = Path::new("/app/./project/./src");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("/app/project/src"));
    }

    #[test]
    fn test_normalize_path_parent_dir() {
        let path = Path::new("/app/project/../project/src");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("/app/project/src"));
    }

    #[test]
    fn test_normalize_path_mixed() {
        let path = Path::new("/app/./project/../workspace/./src");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("/app/workspace/src"));
    }

    #[test]
    fn test_normalize_path_trailing_parent() {
        let path = Path::new("/app/project/..");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("/app"));
    }

    #[test]
    fn test_normalize_path_relative() {
        let path = Path::new("project/./src/../lib");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("project/lib"));
    }
}
