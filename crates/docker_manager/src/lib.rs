use thiserror::Error;

pub mod container_self_inspector;
pub mod container_state_actor;
pub mod container_stop;
pub mod image_selector;
pub mod manager;
pub mod path;
pub mod types;
pub mod utils;

// 新增模块
pub mod container_builder;
pub mod health;
pub mod network;
pub mod runtime_selection;

// Runtime abstraction (Docker/K8s selection)
pub mod runtime;

pub use container_self_inspector::*;
pub use container_state_actor::*;
pub use manager::*;
pub use types::*;
pub use utils::*;

// 公共导出新模块
pub use container_builder::{ContainerConfigBuilder, MountProcessor};
pub use health::{
    HttpHealthChecker, ServiceHealthChecker, ServiceHealthStatus, wait_for_service_ready,
};
pub use network::{NetworkDetector, build_network_name, parse_project_from_network};
pub use path::{
    HostPathResolver, get_host_path_resolver, normalize_path, resolve_container_path_to_host,
};

/// Docker manager error type
#[derive(Error, Debug)]
pub enum DockerError {
    #[error("docker connection failed: {0}")]
    ConnectionError(String),

    #[error("container creation failed: {0}")]
    ContainerCreationError(String),

    #[error("container start failed: {0}")]
    ContainerStartError(String),

    #[error("container stop failed: {0}")]
    ContainerStopError(String),

    #[error("container removal failed: {0}")]
    ContainerRemoveError(String),

    #[error("image pull failed: {0}")]
    ImagePullError(String),

    #[error("configuration error: {0}")]
    ConfigurationError(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("timestamp parsing failed: {0}")]
    InvalidTimestamp(String),

    #[error("docker API call timeout: {0}")]
    Timeout(String),

    #[error("bollard docker error: {0}")]
    BollardError(#[from] bollard::errors::Error),
}

/// Docker 管理器结果类型
pub type DockerResult<T> = Result<T, DockerError>;

/// 默认的 Docker 镜像配置常量
pub mod default_images {
    /// ARM64 架构的默认镜像
    pub const ARM64: &str = "registry.yichamao.com/agent-runner:latest-arm64";

    /// AMD64 架构的默认镜像
    pub const AMD64: &str = "registry.yichamao.com/agent-runner:latest-amd64";

    /// 默认回退镜像（当无法检测架构或架构不匹配时使用）
    pub const DEFAULT: &str = "registry.yichamao.com/agent-runner:latest";
}

/// 默认的 Docker 镜像（根据架构自动选择）
///
/// 注意：此函数使用硬编码的默认值，建议使用 `get_docker_image_from_config()`
/// 从配置中读取镜像地址
pub fn default_docker_image() -> String {
    let platform = crate::utils::DockerUtils::auto_detect_platform();
    match platform.as_str() {
        "linux/arm64" => default_images::ARM64.to_string(),
        "linux/amd64" => default_images::AMD64.to_string(),
        _ => default_images::DEFAULT.to_string(), // 默认回退
    }
}

/// 获取默认的 ARM64 镜像
pub fn default_arm64_image() -> String {
    default_images::ARM64.to_string()
}

/// 获取默认的 AMD64 镜像
pub fn default_amd64_image() -> String {
    default_images::AMD64.to_string()
}

/// 获取默认的回退镜像
pub fn default_fallback_image() -> String {
    default_images::DEFAULT.to_string()
}

/// 从 rcoder 配置获取 Docker 镜像
///
/// # 参数
/// * `image` - 通用镜像（优先使用，如果指定则忽略架构特定镜像）
/// * `arm64_image` - ARM64 架构专用镜像
/// * `amd64_image` - AMD64 架构专用镜像
/// * `default_image` - 默认回退镜像（当无法检测架构或架构不匹配时使用）
pub fn get_docker_image_from_config(
    image: Option<String>,
    arm64_image: Option<String>,
    amd64_image: Option<String>,
    default_image: Option<String>,
) -> String {
    let platform = crate::utils::DockerUtils::auto_detect_platform();

    // 优先使用通用镜像
    if let Some(img) = image {
        return img;
    }

    // 根据架构使用特定镜像
    match platform.as_str() {
        "linux/arm64" => arm64_image.unwrap_or_else(|| {
            default_image.unwrap_or_else(|| default_images::DEFAULT.to_string())
        }),
        "linux/amd64" => amd64_image.unwrap_or_else(|| {
            default_image.unwrap_or_else(|| default_images::DEFAULT.to_string())
        }),
        _ => default_image.unwrap_or_else(|| default_images::DEFAULT.to_string()),
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

/// RCoder 专用网络名称（基础名称，不含 project name 前缀）
/// Docker Compose 会自动添加 project name 前缀，实际网络名称为 {project_name}_{network_name}
/// 例如: rcoder_agent-network, myapp_agent-network
///
/// ⚠️ 注意：实际使用时必须动态检测主容器所在的网络，不能硬编码
pub const RCODER_NETWORK_BASE_NAME: &str = "agent-network";

/// 全局 Docker 管理器实例
pub mod global {
    use super::*;
    #[cfg(feature = "kubernetes")]
    use crate::runtime_selection::RuntimeType;
    use std::sync::Arc;
    use tokio::sync::OnceCell;
    use tracing::{debug, info};

    /// 全局 DockerManager 单例（用于向后兼容）
    static GLOBAL_DOCKER_MANAGER: OnceCell<Arc<DockerManager>> = OnceCell::const_new();

    /// 初始化全局 DockerManager
    pub async fn init_global_docker_manager() -> DockerResult<()> {
        let config = DockerManagerConfig::default();
        let manager = Arc::new(DockerManager::new(config).await?);

        GLOBAL_DOCKER_MANAGER.set(manager).map_err(|_| {
            DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "global DockerManager already initialized",
            ))
        })?;

        info!("DockerManager initialized");
        Ok(())
    }

    /// 使用自定义配置初始化全局 DockerManager
    ///
    /// 注意：此函数保持向后兼容。对于 K8s 支持，请使用 init_global_runtime()
    #[cfg(feature = "kubernetes")]
    pub async fn init_global_docker_manager_with_config(
        config: DockerManagerConfig,
    ) -> DockerResult<()> {
        let runtime_type = RuntimeType::from_env();
        crate::runtime::RuntimeManager::init(config.clone())
            .await
            .map_err(|e| DockerError::ConfigurationError(e.to_string()))?;
        info!("Runtime initialized with config");

        if runtime_type == RuntimeType::Docker {
            let manager = Arc::new(DockerManager::new(config).await?);
            GLOBAL_DOCKER_MANAGER.set(manager).map_err(|_| {
                DockerError::IoError(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "global DockerManager already initialized",
                ))
            })?;
            info!("DockerManager initialized with config");
        }

        Ok(())
    }

    /// 使用自定义配置初始化全局 DockerManager（无 K8s 支持）
    #[cfg(not(feature = "kubernetes"))]
    pub async fn init_global_docker_manager_with_config(
        config: DockerManagerConfig,
    ) -> DockerResult<()> {
        // Initialize RuntimeManager so RUNTIME_INSTANCE is set
        // This allows RuntimeManager::get() to work in docker compose mode
        crate::runtime::RuntimeManager::init(config.clone())
            .await
            .map_err(|e| DockerError::ConfigurationError(e.to_string()))?;
        info!("Runtime initialized with config");

        let manager = Arc::new(DockerManager::new(config).await?);
        GLOBAL_DOCKER_MANAGER.set(manager).map_err(|_| {
            DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "global DockerManager already initialized",
            ))
        })?;
        info!("DockerManager initialized with config");
        Ok(())
    }

    /// 初始化全局运行时（RuntimeManager）
    ///
    /// 根据 CONTAINER_RUNTIME 环境变量选择 Docker 或 Kubernetes 运行时
    #[cfg(feature = "kubernetes")]
    pub async fn init_global_runtime(
        config: DockerManagerConfig,
    ) -> container_runtime_api::ContainerRuntimeResult<()> {
        crate::runtime::RuntimeManager::init(config).await
    }

    /// 获取全局运行时实例（Arc<dyn ContainerRuntime>）
    ///
    /// 支持 Docker 和 Kubernetes 运行时
    #[cfg(feature = "kubernetes")]
    pub async fn get_global_runtime() -> container_runtime_api::ContainerRuntimeResult<
        Arc<dyn container_runtime_api::ContainerRuntime>,
    > {
        crate::runtime::RuntimeManager::get().await
    }

    /// 获取全局 DockerManager 实例
    ///
    /// 注意：此函数仅在 Docker 模式下返回有效的 DockerManager。
    /// 如果需要同时支持 Docker 和 K8s，请使用 get_global_runtime()
    ///
    /// 如果未初始化，会自动初始化
    pub async fn get_global_docker_manager() -> DockerResult<Arc<DockerManager>> {
        #[cfg(feature = "kubernetes")]
        if RuntimeType::from_env() == RuntimeType::Kubernetes {
            return Err(DockerError::ConfigurationError(
                "DockerManager is unavailable in Kubernetes runtime mode. Use RuntimeManager::get()"
                    .to_string(),
            ));
        }

        if GLOBAL_DOCKER_MANAGER.get().is_none() {
            debug!("DockerManager not initialized, starting initialize");
            init_global_docker_manager().await?;
        }

        GLOBAL_DOCKER_MANAGER.get().cloned().ok_or_else(|| {
            DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "unable to get global DockerManager",
            ))
        })
    }

    /// 使用全局 DockerManager 执行操作（向后兼容）
    pub async fn with_global_docker_manager<F, R>(f: F) -> DockerResult<R>
    where
        F: FnOnce(&Arc<DockerManager>) -> R,
    {
        let manager = get_global_docker_manager().await?;
        Ok(f(&manager))
    }
}
