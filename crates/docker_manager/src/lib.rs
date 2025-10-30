use std::collections::HashMap;
use std::path::Path;

use bollard::{
    API_DEFAULT_VERSION, Docker,
    container::{
        Config, CreateContainerOptions, LogsOptions, StartContainerOptions, StopContainerOptions,
    },
    image::CreateImageOptions,
    models::{ContainerCreateResponse, HostConfig, Mount, PortBinding},
};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub mod container_self_inspector;
pub mod container_stop;
pub mod manager;
pub mod types;
pub mod utils;

pub use container_self_inspector::*;
pub use manager::*;
pub use types::*;
pub use utils::*;

/// Docker 管理器错误类型
#[derive(Error, Debug)]
pub enum DockerError {
    #[error("Docker 连接失败: {0}")]
    ConnectionError(String),

    #[error("容器创建失败: {0}")]
    ContainerCreationError(String),

    #[error("容器启动失败: {0}")]
    ContainerStartError(String),

    #[error("容器停止失败: {0}")]
    ContainerStopError(String),

    #[error("容器删除失败: {0}")]
    ContainerRemoveError(String),

    #[error("镜像拉取失败: {0}")]
    ImagePullError(String),

    #[error("IO 错误: {0}")]
    IoError(#[from] std::io::Error),

    #[error("序列化错误: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("Bollard Docker 错误: {0}")]
    BollardError(#[from] bollard::errors::Error),
}

/// Docker 管理器结果类型
pub type DockerResult<T> = Result<T, DockerError>;

/// 默认的 Docker 镜像（根据架构自动选择）
pub fn default_docker_image() -> String {
    let platform = crate::utils::DockerUtils::auto_detect_platform();
    match platform.as_str() {
        "linux/arm64" => "registry.yichamao.com/rcoder:latest-arm64".to_string(),
        "linux/amd64" => "registry.yichamao.com/rcoder:latest-amd64".to_string(),
        _ => "registry.yichamao.com/rcoder:latest".to_string(), // 默认回退
    }
}

/// 从 rcoder 配置获取 Docker 镜像
pub fn get_docker_image_from_config(
    default_image: Option<String>,
    arm64_image: Option<String>,
    amd64_image: Option<String>,
) -> String {
    let platform = crate::utils::DockerUtils::auto_detect_platform();

    // 优先使用通用镜像
    if let Some(image) = default_image {
        return image;
    }

    // 根据架构使用特定镜像
    match platform.as_str() {
        "linux/arm64" => {
            arm64_image.unwrap_or_else(|| "registry.yichamao.com/rcoder:latest-arm64".to_string())
        }
        "linux/amd64" => {
            amd64_image.unwrap_or_else(|| "registry.yichamao.com/rcoder:latest-amd64".to_string())
        }
        _ => "registry.yichamao.com/rcoder:latest".to_string(), // 默认回退
    }
}

/// 默认的平台（使用自动检测）
pub fn default_platform() -> String {
    crate::utils::DockerUtils::auto_detect_platform()
}

/// 默认的工作目录
pub const DEFAULT_WORK_DIR: &str = "/app";

/// 默认的网络模式
pub const DEFAULT_NETWORK_MODE: &str = "bridge";

/// RCoder 专用网络名称
pub const RCODER_NETWORK_NAME: &str = "agent-network";

/// 全局 Docker 管理器实例
pub mod global {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::OnceCell;
    use tracing::{debug, error, info};

    /// 全局 DockerManager 单例
    static GLOBAL_DOCKER_MANAGER: OnceCell<Arc<DockerManager>> = OnceCell::const_new();

    /// 初始化全局 DockerManager
    pub async fn init_global_docker_manager() -> DockerResult<()> {
        let config = DockerManagerConfig::default();
        let manager = Arc::new(DockerManager::new(config).await?);

        GLOBAL_DOCKER_MANAGER.set(manager).map_err(|_| {
            DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "全局 DockerManager 已经初始化",
            ))
        })?;

        info!("✅ 全局 DockerManager 初始化成功");
        Ok(())
    }

    /// 使用自定义配置初始化全局 DockerManager
    pub async fn init_global_docker_manager_with_config(
        config: DockerManagerConfig,
    ) -> DockerResult<()> {
        let manager = Arc::new(DockerManager::new(config).await?);

        GLOBAL_DOCKER_MANAGER.set(manager).map_err(|_| {
            DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "全局 DockerManager 已经初始化",
            ))
        })?;

        info!("✅ 全局 DockerManager 初始化成功（自定义配置）");
        Ok(())
    }

    /// 获取全局 DockerManager 实例
    /// 如果未初始化，会自动初始化
    pub async fn get_global_docker_manager() -> DockerResult<Arc<DockerManager>> {
        if GLOBAL_DOCKER_MANAGER.get().is_none() {
            debug!("全局 DockerManager 未初始化，开始自动初始化");
            init_global_docker_manager().await?;
        }

        GLOBAL_DOCKER_MANAGER.get().cloned().ok_or_else(|| {
            DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "无法获取全局 DockerManager",
            ))
        })
    }

    /// 使用全局 DockerManager 执行操作
    pub async fn with_global_docker_manager<F, R>(f: F) -> DockerResult<R>
    where
        F: FnOnce(&Arc<DockerManager>) -> R,
    {
        let manager = get_global_docker_manager().await?;
        Ok(f(&manager))
    }
}
