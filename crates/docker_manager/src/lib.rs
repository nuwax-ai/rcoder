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

/// 默认的平台（使用自动检测）
pub fn default_platform() -> String {
    crate::utils::DockerUtils::auto_detect_platform()
}

/// 默认的工作目录
pub const DEFAULT_WORK_DIR: &str = "/app/workspace";

/// 默认的网络模式
pub const DEFAULT_NETWORK_MODE: &str = "bridge";

/// RCoder 专用网络名称
pub const RCODER_NETWORK_NAME: &str = "rcoder-network";
