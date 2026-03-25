//! 挂载点处理器
//!
//! 处理容器挂载点的路径解析和变量替换

use crate::path::HostPathResolver;
use crate::{DockerResult, MountPoint};
use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, info};

/// 挂载点处理器
///
/// 提供挂载点路径解析、变量替换等功能
pub struct MountProcessor {
    resolver: HostPathResolver,
}

impl MountProcessor {
    /// 创建新的挂载点处理器
    ///
    /// # Arguments
    /// * `resolver` - 路径解析器
    pub fn new(resolver: HostPathResolver) -> Self {
        Self { resolver }
    }

    /// 创建新的挂载点处理器（异步，自动创建路径解析器）
    ///
    /// # Returns
    /// * `DockerResult<Self>` - 挂载点处理器或错误
    pub async fn new_async() -> DockerResult<Self> {
        let resolver = HostPathResolver::new().await?;
        Ok(Self { resolver })
    }

    /// 使用指定的 Docker socket 创建挂载点处理器
    ///
    /// # Arguments
    /// * `docker_socket_path` - Docker socket 路径
    ///
    /// # Returns
    /// * `DockerResult<Self>` - 挂载点处理器或错误
    pub async fn new_with_docker_socket(
        docker_socket_path: Option<String>,
    ) -> DockerResult<Self> {
        let resolver = HostPathResolver::new_with_docker_socket(docker_socket_path).await?;
        Ok(Self { resolver })
    }

    /// 处理单个挂载点
    ///
    /// # Arguments
    /// * `container_path` - 容器内路径
    /// * `host_path` - 宿主机路径（可能包含变量或相对路径）
    /// * `read_only` - 是否只读
    /// * `variables` - 变量映射表（可选）
    ///
    /// # Returns
    /// * `DockerResult<MountPoint>` - 处理后的挂载点或错误
    pub fn process_mount(
        &self,
        container_path: impl AsRef<str>,
        host_path: impl AsRef<str>,
        read_only: bool,
        variables: Option<&HashMap<String, String>>,
    ) -> DockerResult<MountPoint> {
        let container_path = container_path.as_ref();
        let mut host_path = host_path.as_ref().to_string();

        debug!(
            "处理挂载点: {} -> {} (只读: {})",
            container_path, host_path, read_only
        );

        // 变量替换
        if let Some(vars) = variables {
            for (key, value) in vars {
                let pattern = format!("{{{}}}", key);
                if host_path.contains(&pattern) {
                    host_path = host_path.replace(&pattern, value);
                    debug!("变量替换: {} -> {}", pattern, value);
                }
            }
        }

        // 路径解析
        let normalized_host_path = self.resolve_path(&host_path)?;

        info!(
            "✅ 挂载点处理完成: {} -> {}",
            container_path, normalized_host_path
        );

        Ok(MountPoint {
            container_path: container_path.to_string(),
            host_path: normalized_host_path,
            read_only,
        })
    }

    /// 批量处理挂载点
    ///
    /// # Arguments
    /// * `mounts` - 挂载点列表 (container_path, host_path, read_only)
    /// * `variables` - 变量映射表（可选）
    ///
    /// # Returns
    /// * `DockerResult<Vec<MountPoint>>` - 处理后的挂载点列表或错误
    pub fn process_mounts(
        &self,
        mounts: Vec<(String, String, bool)>,
        variables: Option<&HashMap<String, String>>,
    ) -> DockerResult<Vec<MountPoint>> {
        debug!("批量处理 {} 个挂载点", mounts.len());

        let processed: DockerResult<Vec<MountPoint>> = mounts
            .into_iter()
            .map(|(container_path, host_path, read_only)| {
                self.process_mount(&container_path, &host_path, read_only, variables)
            })
            .collect();

        let processed = processed?;
        info!("批量挂载点处理完成: {} 个", processed.len());

        Ok(processed)
    }

    /// 解析路径（处理相对路径和容器内路径）
    ///
    /// # Arguments
    /// * `path` - 待解析的路径
    ///
    /// # Returns
    /// * `DockerResult<String>` - 解析后的宿主机绝对路径
    fn resolve_path(&self, path: &str) -> DockerResult<String> {
        let path_obj = Path::new(path);

        // 处理相对路径：转换为容器内绝对路径
        let container_absolute_path = if path_obj.is_relative() {
            let current_dir = std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("/app"));
            current_dir.join(path_obj)
        } else {
            path_obj.to_path_buf()
        };

        // 检查是否为容器内路径
        if container_absolute_path.starts_with("/app") {
            // 容器内路径：转换为宿主机路径
            debug!("检测到容器内路径，进行路径解析: {}", container_absolute_path.display());
            let host_abs_path = self.resolver.resolve_to_host_path(&container_absolute_path)?;
            Ok(host_abs_path.to_string_lossy().to_string())
        } else {
            // 可能已经是宿主机路径，直接使用
            debug!("使用可能是宿主机的路径: {}", container_absolute_path.display());
            Ok(container_absolute_path.to_string_lossy().to_string())
        }
    }

    /// 获取路径解析器的引用
    pub fn resolver(&self) -> &HostPathResolver {
        &self.resolver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mount_processor_variable_substitution() {
        // 注意：此测试需要在容器环境中运行才能完整验证路径解析
        // 这里仅测试变量替换逻辑

        let mut variables = HashMap::new();
        variables.insert("project_id".to_string(), "test-123".to_string());

        let host_path = "/path/to/{project_id}/data";
        let mut result = host_path.to_string();

        for (key, value) in &variables {
            let pattern = format!("{{{}}}", key);
            result = result.replace(&pattern, value);
        }

        assert_eq!(result, "/path/to/test-123/data");
    }

    #[test]
    fn test_mount_point_structure() {
        let mount = MountPoint {
            container_path: "/app/data".to_string(),
            host_path: "/host/data".to_string(),
            read_only: false,
        };

        assert_eq!(mount.container_path, "/app/data");
        assert_eq!(mount.host_path, "/host/data");
        assert!(!mount.read_only);
    }

    // 注意：以下测试需要在 Docker 容器环境中运行
    #[tokio::test]
    #[ignore]
    async fn test_mount_processor_creation() {
        // 此测试仅在容器内有效
        if std::env::var("HOSTNAME").is_ok() {
            let result = MountProcessor::new_async().await;
            assert!(result.is_ok());
        }
    }
}
