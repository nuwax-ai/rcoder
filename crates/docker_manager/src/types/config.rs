//! Docker 管理器配置。

use serde::{Deserialize, Serialize};

/// Docker 管理器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerManagerConfig {
    /// Docker 守护进程地址
    pub docker_host: Option<String>,
    /// 默认镜像
    pub default_image: String,
    /// 默认平台
    pub default_platform: String,
    /// 默认网络模式
    pub default_network_mode: String,
    /// 默认工作目录
    pub default_work_dir: String,
    /// 是否启用自动清理
    pub auto_cleanup: bool,
    /// 容器存活时间 (秒)
    pub container_ttl_seconds: Option<u64>,

    /// 多镜像配置（从 rcoder 配置传递，始终有值）
    pub multi_image_config: shared_types::MultiImageConfig,

    /// K8s 运行时专用配置(从 rcoder 配置传递;docker 模式下为空默认值,不被读取)
    ///
    /// 与 `multi_image_config`(docker 用)分家:K8s 构建器只读本字段,
    /// docker 运行时只读 `multi_image_config`。docker 部署下为 `KubernetesConfig::default()`(空)。
    #[serde(default)]
    pub kubernetes_config: shared_types::KubernetesConfig,

    /// 网络基础名称（不含 project name 前缀）
    /// Docker Compose 会自动添加 project name 前缀，实际网络名称为 {project_name}_{network_base_name}
    /// 例如: network_base_name="agent-network" 时，实际网络为 "rcoder_agent-network"
    pub network_base_name: String,

    /// 🔧 Docker API 调用超时时间（秒）
    /// 用于大多数 Docker API 调用的超时保护
    #[serde(default = "default_api_timeout")]
    pub api_timeout_seconds: u64,

    /// 🔧 快速操作超时时间（秒）
    /// 用于状态查询等轻量级操作的超时保护
    #[serde(default = "default_api_timeout_quick")]
    pub api_timeout_quick_seconds: u64,

    /// 🔧 状态缓存 TTL（秒）
    /// 用于缓存容器状态信息（container_id, container_name, status, is_running）
    #[serde(default = "default_cache_status_ttl")]
    pub cache_status_ttl_seconds: u64,

    /// 🔧 网络缓存 TTL（秒）
    /// 用于缓存容器网络信息（network_name -> ip_address）
    #[serde(default = "default_cache_network_ttl")]
    pub cache_network_ttl_seconds: u64,

    /// 🔧 缓存最大容量
    /// 用于 DockerApiCache 的 status_cache 和 network_cache
    #[serde(default = "default_cache_max_capacity")]
    pub cache_max_capacity: u64,
}

/// 默认 API 超时时间（10秒）
fn default_api_timeout() -> u64 {
    10
}

/// 默认快速操作超时时间（5秒）
fn default_api_timeout_quick() -> u64 {
    5
}

/// 默认状态缓存 TTL（10秒）
fn default_cache_status_ttl() -> u64 {
    10
}

/// 默认网络缓存 TTL（15秒）
fn default_cache_network_ttl() -> u64 {
    15
}

/// 默认缓存最大容量（10000）
fn default_cache_max_capacity() -> u64 {
    10000
}

/// Docker 配置（从 rcoder 配置传递）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerConfig {
    /// Docker 镜像名称（根据架构自动选择）
    pub image: Option<String>,
    /// ARM64 架构的 Docker 镜像
    pub arm64_image: Option<String>,
    /// AMD64 架构的 Docker 镜像
    pub amd64_image: Option<String>,
    /// 默认回退镜像（当无法检测架构或架构不匹配时使用）
    pub default_image: Option<String>,
    /// 默认网络模式
    pub network_mode: Option<String>,
    /// 默认工作目录
    pub work_dir: Option<String>,
    /// 是否启用自动清理
    pub auto_cleanup: Option<bool>,
    /// 容器存活时间（秒）
    pub container_ttl_seconds: Option<u64>,
}

impl Default for DockerManagerConfig {
    fn default() -> Self {
        Self {
            docker_host: None, // 使用默认的 Docker socket
            default_image: crate::default_docker_image(),
            default_platform: crate::default_platform(),
            default_network_mode: crate::DEFAULT_NETWORK_MODE.to_string(),
            default_work_dir: crate::DEFAULT_WORK_DIR.to_string(),
            auto_cleanup: true,
            container_ttl_seconds: Some(3600), // 1小时

            multi_image_config: shared_types::create_default_multi_image_config(), // 默认多镜像配置
            kubernetes_config: shared_types::KubernetesConfig::default(), // K8s 模式从 config.yml 填充;docker 模式空默认
            network_base_name: crate::RCODER_NETWORK_BASE_NAME.to_string(), // 默认网络基础名称

            api_timeout_seconds: default_api_timeout(), // 默认 10 秒
            api_timeout_quick_seconds: default_api_timeout_quick(), // 默认 5 秒

            cache_status_ttl_seconds: default_cache_status_ttl(), // 默认 10 秒
            cache_network_ttl_seconds: default_cache_network_ttl(), // 默认 15 秒
            cache_max_capacity: default_cache_max_capacity(),     // 默认 10000
        }
    }
}
