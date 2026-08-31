//! 配置子段定义（从 config/mod.rs 拆出——AppConfig 的嵌套子 struct 群）。
//!
//! 各子段与其私有 serde default fn **整体同档**（字段级 `#[serde(default = "...")]`
//! 绑定不因拆分变化）；DockerConfig 的镜像选择/校验方法群（四级兜底）随 struct 同档。

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    /// 是否启用健康检查
    pub enabled: bool,
    /// 检查间隔（秒）
    pub interval_seconds: u64,
    /// 超时时间（秒）
    pub timeout_seconds: u64,
    /// 健康阈值
    pub healthy_threshold: u32,
    /// 不健康阈值
    pub unhealthy_threshold: u32,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_seconds: 5,
            timeout_seconds: 1,
            healthy_threshold: 2,
            unhealthy_threshold: 3,
        }
    }
}

/// 代理 HTTP 客户端配置（用于协议转换时连接上游 API）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyHttpClientConfig {
    /// 请求超时（秒），默认 600（10 分钟，AI 请求可能很长）
    #[serde(default = "default_http_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    /// 连接建立超时（秒），默认 10
    #[serde(default = "default_http_connect_timeout_seconds")]
    pub connect_timeout_seconds: u64,
    /// 连接池空闲超时（秒），默认 90
    #[serde(default = "default_http_pool_idle_timeout_seconds")]
    pub pool_idle_timeout_seconds: u64,
}

fn default_http_request_timeout_seconds() -> u64 {
    600
}
fn default_http_connect_timeout_seconds() -> u64 {
    10
}
fn default_http_pool_idle_timeout_seconds() -> u64 {
    90
}

impl Default for ProxyHttpClientConfig {
    fn default() -> Self {
        Self {
            request_timeout_seconds: 600,
            connect_timeout_seconds: 10,
            pool_idle_timeout_seconds: 90,
        }
    }
}

/// 反向代理配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// 代理服务监听端口
    pub listen_port: u16,
    /// 默认后端服务端口
    pub default_backend_port: u16,
    /// 后端服务主机地址
    pub backend_host: String,
    /// 端口参数名称
    pub port_param: String,
    /// 健康检查配置
    pub health_check: HealthCheckConfig,
    /// HTTP 客户端配置（用于协议转换时连接上游 API）
    #[serde(default)]
    pub http_client: ProxyHttpClientConfig,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: 8088,
            default_backend_port: 8086,
            backend_host: "127.0.0.1".to_string(),
            port_param: "port".to_string(),
            health_check: HealthCheckConfig::default(),
            http_client: ProxyHttpClientConfig::default(),
        }
    }
}

/// 日志清理配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogCleanupConfig {
    /// 日志目录路径
    #[serde(default = "default_log_dir")]
    pub log_dir: String,
    /// 日志保留天数，默认10天
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u64,
}

fn default_log_dir() -> String {
    "/app/logs/container".to_string()
}

fn default_log_retention_days() -> u64 {
    10
}

impl Default for LogCleanupConfig {
    fn default() -> Self {
        Self {
            log_dir: default_log_dir(),
            log_retention_days: default_log_retention_days(),
        }
    }
}

/// 容器清理配置（配置文件格式，使用秒作为单位）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupConfigSettings {
    /// 是否启用容器清理功能（默认禁用，更安全）
    #[serde(default = "default_cleanup_enabled")]
    pub enabled: bool,
    /// 闲置超时时间（秒），默认600秒（10分钟）
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
    /// 清理检查间隔（秒），默认300秒（5分钟）
    #[serde(default = "default_cleanup_interval_seconds")]
    pub cleanup_interval_seconds: u64,
    /// Docker容器停止超时时间（秒），默认30秒
    #[serde(default = "default_docker_stop_timeout_seconds")]
    pub docker_stop_timeout_seconds: u64,
    /// 容器最小保护时间（秒），默认300秒（5分钟）
    #[serde(default = "default_container_protection_seconds")]
    pub container_protection_seconds: u64,
    /// 长期闲置超时时间（秒），超过此值才真正销毁容器 + 删 project（默认 86400 = 24 小时）。
    /// 短期闲置（idle_timeout ~ long_idle_timeout）只标记 Idle、保留容器复用。
    #[serde(default = "default_long_idle_timeout_seconds")]
    pub long_idle_timeout_seconds: u64,
    /// 日志清理配置
    #[serde(default)]
    pub log_cleanup: LogCleanupConfig,
}

fn default_cleanup_enabled() -> bool {
    true // 默认启用容器清理功能
}

fn default_idle_timeout_seconds() -> u64 {
    600 // 10分钟
}

fn default_cleanup_interval_seconds() -> u64 {
    300 // 5分钟
}

fn default_docker_stop_timeout_seconds() -> u64 {
    30
}

fn default_container_protection_seconds() -> u64 {
    300 // 5分钟
}

fn default_long_idle_timeout_seconds() -> u64 {
    3600 // 1小时 — 短期闲置保留复用，长期闲置才销毁
}

impl Default for CleanupConfigSettings {
    fn default() -> Self {
        Self {
            enabled: default_cleanup_enabled(),
            idle_timeout_seconds: default_idle_timeout_seconds(),
            cleanup_interval_seconds: default_cleanup_interval_seconds(),
            docker_stop_timeout_seconds: default_docker_stop_timeout_seconds(),
            container_protection_seconds: default_container_protection_seconds(),
            long_idle_timeout_seconds: default_long_idle_timeout_seconds(),
            log_cleanup: LogCleanupConfig::default(),
        }
    }
}

/// Userapp 闲置自动回收 + 流量唤醒配置（配置文件格式，秒为单位）
///
/// 默认 `enabled=true`：免费用户 app 闲置超阈值自动 scale0 回收；付费 app 经
/// `CreateAppRequest.recycle_enabled=false`（注解 `rcoder.io/recycle-enabled=false`）opt-out 永不回收。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserAppRecycleConfig {
    /// 是否启用自动回收 + 流量唤醒（默认 true；部署侧可 env/helm 关闭）
    #[serde(default = "default_userapp_recycle_enabled")]
    pub enabled: bool,
    /// 闲置超时阈值（秒），默认 432000（5 天）
    #[serde(default = "default_userapp_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
    /// 回收扫描间隔（秒），默认 3600（1 小时）
    #[serde(default = "default_userapp_scan_interval_seconds")]
    pub scan_interval_seconds: u64,
    /// 流量唤醒 hold-and-wait 上限（秒），默认 60；超时返回 503+Retry-After
    #[serde(default = "default_userapp_wake_timeout_seconds")]
    pub wake_timeout_seconds: u64,
    /// 新建 app 最小保护期（秒），默认 300；龄期小于此值不回收
    #[serde(default = "default_userapp_protection_seconds")]
    pub protection_seconds: u64,
}

fn default_userapp_recycle_enabled() -> bool {
    true // 默认免费用户自动回收
}

fn default_userapp_idle_timeout_seconds() -> u64 {
    432000 // 5 天
}

fn default_userapp_scan_interval_seconds() -> u64 {
    3600 // 1 小时
}

fn default_userapp_wake_timeout_seconds() -> u64 {
    60
}

fn default_userapp_protection_seconds() -> u64 {
    300 // 5 分钟
}

/// 从环境变量读取 bool 覆盖 target;解析失败 warn 不阻塞(Fail Fast 仅记日志,沿用默认)。
pub(super) fn env_override_bool(key: &str, target: &mut bool) {
    if let Ok(val) = std::env::var(key)
        && let Ok(v) = val.parse::<bool>()
    {
        *target = v;
        info!(" {key}: {v}");
    } else if let Ok(val) = std::env::var(key) {
        warn!(" parse {key} failed: {val}");
    }
}

/// 从环境变量读取 u64 覆盖 target。
pub(super) fn env_override_u64(key: &str, target: &mut u64) {
    if let Ok(val) = std::env::var(key)
        && let Ok(v) = val.parse::<u64>()
    {
        *target = v;
        info!(" {key}: {v}");
    } else if let Ok(val) = std::env::var(key) {
        warn!(" parse {key} failed: {val}");
    }
}

impl Default for UserAppRecycleConfig {
    fn default() -> Self {
        Self {
            enabled: default_userapp_recycle_enabled(),
            idle_timeout_seconds: default_userapp_idle_timeout_seconds(),
            scan_interval_seconds: default_userapp_scan_interval_seconds(),
            wake_timeout_seconds: default_userapp_wake_timeout_seconds(),
            protection_seconds: default_userapp_protection_seconds(),
        }
    }
}

/// Docker 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerConfig {
    /// 多镜像配置
    pub multi_image_config: Option<shared_types::MultiImageConfig>,
    /// 网络模式
    pub network_mode: Option<String>,
    /// 工作目录
    pub work_dir: Option<String>,
    /// 自动清理
    pub auto_cleanup: Option<bool>,
    /// 容器存活时间（秒）
    pub container_ttl_seconds: Option<u64>,
    /// 网络基础名称（不含 project name 前缀）
    /// Docker Compose 会自动添加 project name 前缀，实际网络名称为 {project_name}_{network_base_name}
    /// 例如: network_base_name="agent-network" 时，实际网络为 "rcoder_agent-network"
    pub network_base_name: Option<String>,
    /// 🔧 Docker API 调用超时时间（秒）
    pub api_timeout_seconds: Option<u64>,
    /// 🔧 快速操作超时时间（秒）
    pub api_timeout_quick_seconds: Option<u64>,
    /// 🔧 状态缓存 TTL（秒）
    pub cache_status_ttl_seconds: Option<u64>,
    /// 🔧 网络缓存 TTL（秒）
    pub cache_network_ttl_seconds: Option<u64>,
    /// 🔧 缓存最大容量
    pub cache_max_capacity: Option<u64>,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            multi_image_config: Some(shared_types::create_default_multi_image_config()),
            network_mode: Some("bridge".to_string()),
            work_dir: Some("/app".to_string()),
            auto_cleanup: Some(true),
            container_ttl_seconds: Some(3600),
            network_base_name: Some("agent-network".to_string()),
            // 🔧 新增字段默认值
            api_timeout_seconds: Some(10),
            api_timeout_quick_seconds: Some(5),
            cache_status_ttl_seconds: Some(10),
            cache_network_ttl_seconds: Some(15),
            cache_max_capacity: Some(10000),
        }
    }
}

impl DockerConfig {
    /// 获取多镜像配置，如果没有配置多镜像配置，会从传统配置自动转换
    pub fn get_multi_image_config(&self) -> shared_types::MultiImageConfig {
        if let Some(ref multi_config) = self.multi_image_config {
            multi_config.clone()
        } else {
            // 从传统配置创建多镜像配置
            self.create_legacy_multi_config()
        }
    }

    /// 从传统配置创建多镜像配置
    fn create_legacy_multi_config(&self) -> shared_types::MultiImageConfig {
        info!("Config created from legacy config");

        // 创建基于传统配置的多镜像配置
        let mut services = std::collections::HashMap::new();

        // 为 RCoder 服务使用默认配置
        let rcoder_service = {
            info!("Using default config");
            shared_types::default_rcoder_service_config()
        };

        services.insert("web-agent-runner".to_string(), rcoder_service);

        // 为 AgentRunner 服务使用默认配置
        services.insert(
            "agent-runner".to_string(),
            shared_types::default_agent_runner_service_config(),
        );

        shared_types::MultiImageConfig {
            services,
            global_defaults: shared_types::GlobalImageDefaults {
                image: None,
                arm64_image: None,
                amd64_image: None,
                default_image: None,
                registry_prefix: None,
            },
            selection_strategy: shared_types::ImageSelectionStrategy::ServiceOnly,
            cache_config: shared_types::ImageCacheConfig {
                enabled: true,
                ttl_seconds: shared_types::IMAGE_CACHE_DEFAULT_TTL_SECS,
                // rcoder 主进程面向多 project，缓存容量高于 crate 默认值
                max_entries: 100,
            },
        }
    }

    /// 验证多镜像配置
    pub fn validate_multi_image_config(&self) -> Result<(), String> {
        let multi_config = self.get_multi_image_config();
        match multi_config.validate() {
            Ok(()) => {
                tracing::info!("[CONFIG] Multi-image config validation passed");
                Ok(())
            }
            Err(e) => {
                tracing::error!("[CONFIG] Multi-image config validation failed: {}", e);
                // 打印每个服务的配置详情
                for (service_key, service_config) in &multi_config.services {
                    tracing::error!(
                        "[CONFIG]   Service '{}': service_type={}, image={:?}, arm64_image={:?}, amd64_image={:?}, default_image={:?}, enabled={}",
                        service_key,
                        service_config.service_type,
                        service_config.image,
                        service_config.arm64_image,
                        service_config.amd64_image,
                        service_config.default_image,
                        service_config.enabled
                    );
                }
                Err(e.to_string())
            }
        }
    }

    /// 检查是否使用多镜像配置
    pub fn is_using_multi_image_config(&self) -> bool {
        self.multi_image_config.is_some()
    }

    /// 应用环境变量覆盖
    pub fn apply_env_overrides(&mut self) -> anyhow::Result<()> {
        // 应用网络模式
        if let Ok(val) = std::env::var("RCODER_NETWORK_MODE") {
            info!("RCODER_NETWORK_MODE overridden");
            self.network_mode = Some(val);
        }

        // 应用网络基础名称
        if let Ok(val) = std::env::var("RCODER_NETWORK_BASE_NAME") {
            info!("RCODER_NETWORK_BASE_NAME: {}", val);
            self.network_base_name = Some(val);
        }

        // 应用工作目录
        if let Ok(val) = std::env::var("RCODER_WORK_DIR") {
            info!("RCODER_WORK_DIR overridden");
            self.work_dir = Some(val);
        }

        // 应用自动清理
        if let Ok(val) = std::env::var("RCODER_AUTO_CLEANUP") {
            info!("RCODER_AUTO_CLEANUP overridden");
            self.auto_cleanup = Some(val.parse().unwrap_or(true));
        }

        // 应用容器存活时间
        if let Ok(val) = std::env::var("RCODER_CONTAINER_TTL") {
            info!("RCODER_CONTAINER_TTL overridden");
            match val.parse() {
                Ok(seconds) => self.container_ttl_seconds = Some(seconds),
                Err(e) => {
                    tracing::warn!(
                        "[CONFIG] Failed to parse RCODER_CONTAINER_TTL '{}': {}, using default",
                        val,
                        e
                    );
                }
            }
        }

        // 🔧 应用 API 超时配置
        if let Ok(val) = std::env::var("RCODER_API_TIMEOUT_SECONDS") {
            info!("RCODER_API_TIMEOUT_SECONDS overridden");
            match val.parse() {
                Ok(seconds) => self.api_timeout_seconds = Some(seconds),
                Err(e) => {
                    tracing::warn!(
                        "[CONFIG] Failed to parse RCODER_API_TIMEOUT_SECONDS '{}': {}, using default",
                        val,
                        e
                    );
                }
            }
        }

        // 🔧 应用快速操作超时配置
        if let Ok(val) = std::env::var("RCODER_API_TIMEOUT_QUICK_SECONDS") {
            info!("RCODER_API_TIMEOUT_QUICK_SECONDS overridden");
            match val.parse() {
                Ok(seconds) => self.api_timeout_quick_seconds = Some(seconds),
                Err(e) => {
                    tracing::warn!(
                        "[CONFIG] Failed to parse RCODER_API_TIMEOUT_QUICK_SECONDS '{}': {}, using default",
                        val,
                        e
                    );
                }
            }
        }

        // 🔧 应用状态缓存 TTL 配置
        if let Ok(val) = std::env::var("RCODER_CACHE_STATUS_TTL_SECONDS") {
            info!("RCODER_CACHE_STATUS_TTL_SECONDS overridden");
            match val.parse() {
                Ok(seconds) => self.cache_status_ttl_seconds = Some(seconds),
                Err(e) => {
                    tracing::warn!(
                        "[CONFIG] Failed to parse RCODER_CACHE_STATUS_TTL_SECONDS '{}': {}, using default",
                        val,
                        e
                    );
                }
            }
        }

        // 🔧 应用网络缓存 TTL 配置
        if let Ok(val) = std::env::var("RCODER_CACHE_NETWORK_TTL_SECONDS") {
            info!("RCODER_CACHE_NETWORK_TTL_SECONDS overridden");
            match val.parse() {
                Ok(seconds) => self.cache_network_ttl_seconds = Some(seconds),
                Err(e) => {
                    tracing::warn!(
                        "[CONFIG] Failed to parse RCODER_CACHE_NETWORK_TTL_SECONDS '{}': {}, using default",
                        val,
                        e
                    );
                }
            }
        }

        // 🔧 应用缓存最大容量配置
        if let Ok(val) = std::env::var("RCODER_CACHE_MAX_CAPACITY") {
            info!("RCODER_CACHE_MAX_CAPACITY overridden");
            match val.parse() {
                Ok(capacity) => self.cache_max_capacity = Some(capacity),
                Err(e) => {
                    tracing::warn!(
                        "[CONFIG] Failed to parse RCODER_CACHE_MAX_CAPACITY '{}': {}, using default",
                        val,
                        e
                    );
                }
            }
        }

        Ok(())
    }

    /// Get configuration summary
    pub fn get_summary(&self) -> String {
        format!(
            "Docker config: network_mode={}, network_base_name={}, work_dir={}, auto_cleanup={}, container_ttl={}, api_timeout={}s, quick_timeout={}s, status_cache={}s, network_cache={}s, cache_max_capacity={}",
            self.network_mode.as_deref().unwrap_or("default"),
            self.network_base_name.as_deref().unwrap_or("agent-network"),
            self.work_dir.as_deref().unwrap_or("/app"),
            self.auto_cleanup.unwrap_or(true),
            self.container_ttl_seconds.unwrap_or(3600),
            self.api_timeout_seconds.unwrap_or(10),
            self.api_timeout_quick_seconds.unwrap_or(5),
            self.cache_status_ttl_seconds.unwrap_or(10),
            self.cache_network_ttl_seconds.unwrap_or(15),
            self.cache_max_capacity.unwrap_or(10000)
        )
    }
}
