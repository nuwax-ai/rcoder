//! 路径解析工具函数
//!
//! 提供便捷的路径解析接口

use crate::DockerResult;
use crate::path::HostPathResolver;
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
/// # 安全行为
/// - **绝对路径**: 根目录处的 `..` 被忽略（无法逃逸根目录）
/// - **相对路径**: 前导 `..` 被保留，调用方可据此检测路径逃逸尝试
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
///
/// // 相对路径中的前导 `..` 被保留
/// let normalized = normalize_path(Path::new("../../../etc/passwd"));
/// assert_eq!(normalized, Path::new("../../../etc/passwd"));
/// ```
pub fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut components = Vec::new();
    let mut is_absolute = false;

    for component in path.components() {
        match component {
            Component::RootDir => {
                is_absolute = true;
                components.push(component);
            }
            #[cfg(windows)]
            Component::Prefix(_) => {
                is_absolute = true;
                components.push(component);
            }
            Component::CurDir => {
                // 跳过 `.`
            }
            Component::ParentDir => {
                // 处理 `..`
                match components.last() {
                    // 已有正常组件可以回退（不能回退到 RootDir 或 Prefix 之前）
                    Some(Component::Normal(_)) => {
                        components.pop();
                    }
                    // 相对路径：保留前导 `..`，供调用方检测路径逃逸
                    None | Some(Component::ParentDir) if !is_absolute => {
                        components.push(component);
                    }
                    // 绝对路径根目录处的 `..` 或紧跟 `..` 后的 `..`：忽略
                    _ => {}
                }
            }
            other => {
                components.push(other);
            }
        }
    }

    if components.is_empty() {
        PathBuf::from(".")
    } else {
        components.iter().collect()
    }
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

    #[test]
    fn test_normalize_path_absolute_root_parent() {
        // 绝对路径根目录处的 `..` 被忽略，不逃逸根目录
        let path = Path::new("/../etc/passwd");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("/etc/passwd"));
    }

    #[test]
    fn test_normalize_path_relative_leading_parent_preserved() {
        // 相对路径的前导 `..` 被保留，供调用方检测路径逃逸
        let path = Path::new("../../../etc/passwd");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("../../../etc/passwd"));
    }

    #[test]
    fn test_normalize_path_relative_mixed_parents() {
        // 相对路径中混合 `..` 和正常组件
        let path = Path::new("../foo/../bar");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("../bar"));
    }

    #[test]
    fn test_normalize_path_empty_relative() {
        let path = Path::new(".");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("."));
    }

    #[test]
    fn test_normalize_path_only_parents() {
        let path = Path::new("../..");
        let normalized = normalize_path(path);
        assert_eq!(normalized, Path::new("../.."));
    }
}
