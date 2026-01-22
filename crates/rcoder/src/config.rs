use std::fs;
use std::path::PathBuf;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// 命令行参数
#[derive(Parser, Debug)]
#[command(name = "rcoder")]
#[command(about = "RCoder - Rust-based AI Agent Framework")]
#[command(version)]
pub struct CliArgs {
    /// 主服务端口
    #[arg(short = 'p', long)]
    pub port: Option<u16>,

    /// 项目工作目录
    #[arg(short = 'd', long, default_value = "./project_workspace")]
    pub projects_dir: Option<String>,

    /// 启用反向代理
    #[arg(short, long)]
    pub enable_proxy: bool,

    /// 代理服务端口
    #[arg(long = "proxy-port")]
    pub proxy_port: Option<u16>,

    /// 默认后端服务端口
    #[arg(long = "backend-port")]
    pub default_backend_port: Option<u16>,
}

// 从 shared_types 导入 API Key 鉴权配置
pub use shared_types::ApiKeyAuthConfig;

/// 应用程序配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 默认使用的 Agent ID
    #[serde(default = "default_agent_id")]
    pub default_agent_id: String,
    /// 项目工作目录
    pub projects_dir: PathBuf,
    /// 主服务端口
    pub port: u16,
    /// 反向代理配置
    pub proxy_config: Option<ProxyConfig>,
    /// Docker 配置
    pub docker_config: Option<DockerConfig>,
    /// 容器清理配置
    #[serde(default)]
    pub cleanup_config: CleanupConfigSettings,
    /// API Key 鉴权配置
    #[serde(default)]
    pub api_key_auth: ApiKeyAuthConfig,
}

fn default_agent_id() -> String {
    "claude-code-acp".to_string()
}

/// 生成随机 API Key
/// 使用 UUID v4 生成随机密钥，格式：sk-{uuid}
fn generate_random_api_key() -> String {
    use uuid::Uuid;
    let uuid = Uuid::new_v4();
    format!("sk-{}", uuid.simple())
}

/// 健康检查配置
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
}

/// 日志清理配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogCleanupConfig {
    /// 日志目录路径
    #[serde(default = "default_log_dir")]
    pub log_dir: String,
    /// 日志保留天数，默认7天
    #[serde(default = "default_log_retention_days")]
    pub log_retention_days: u64,
}

fn default_log_dir() -> String {
    "/app/logs/container".to_string()
}

fn default_log_retention_days() -> u64 {
    7
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
    /// 日志清理配置
    #[serde(default)]
    pub log_cleanup: LogCleanupConfig,
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

impl Default for CleanupConfigSettings {
    fn default() -> Self {
        Self {
            idle_timeout_seconds: default_idle_timeout_seconds(),
            cleanup_interval_seconds: default_cleanup_interval_seconds(),
            docker_stop_timeout_seconds: default_docker_stop_timeout_seconds(),
            container_protection_seconds: default_container_protection_seconds(),
            log_cleanup: LogCleanupConfig::default(),
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
}

pub const CONFIG_FILE: &str = "config.yml";

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_agent_id: default_agent_id(),
            projects_dir: PathBuf::from("./project_workspace"),
            port: 8087,
            proxy_config: Some(ProxyConfig::default()),
            docker_config: Some(DockerConfig::default()),
            cleanup_config: CleanupConfigSettings::default(),
            api_key_auth: ApiKeyAuthConfig {
                enabled: false,
                api_key: generate_random_api_key(),
            },
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: 8088,
            default_backend_port: 8086,
            backend_host: "127.0.0.1".to_string(),
            port_param: "port".to_string(),
            health_check: HealthCheckConfig::default(),
        }
    }
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
        info!("从传统配置创建多镜像配置");

        // 创建基于传统配置的多镜像配置
        let mut services = std::collections::HashMap::new();

        // 为 RCoder 服务使用默认配置
        let rcoder_service = {
            info!("使用默认镜像配置");
            shared_types::service_config::default_rcoder_service_config()
        };

        services.insert("rcoder".to_string(), rcoder_service);

        // 为 AgentRunner 服务使用默认配置
        services.insert(
            "agent-runner".to_string(),
            shared_types::service_config::default_agent_runner_service_config(),
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
                ttl_seconds: 3600,
                max_entries: 100,
            },
        }
    }

    /// 验证多镜像配置
    pub fn validate_multi_image_config(&self) -> Result<(), String> {
        let multi_config = self.get_multi_image_config();
        match multi_config.validate() {
            Ok(()) => Ok(()),
            Err(e) => Err(e.to_string()),
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
            info!("应用环境变量 RCODER_NETWORK_MODE");
            self.network_mode = Some(val);
        }

        // 应用网络基础名称
        if let Ok(val) = std::env::var("RCODER_NETWORK_BASE_NAME") {
            info!("应用环境变量 RCODER_NETWORK_BASE_NAME: {}", val);
            self.network_base_name = Some(val);
        }

        // 应用工作目录
        if let Ok(val) = std::env::var("RCODER_WORK_DIR") {
            info!("应用环境变量 RCODER_WORK_DIR");
            self.work_dir = Some(val);
        }

        // 应用自动清理
        if let Ok(val) = std::env::var("RCODER_AUTO_CLEANUP") {
            info!("应用环境变量 RCODER_AUTO_CLEANUP");
            self.auto_cleanup = Some(val.parse().unwrap_or(true));
        }

        // 应用容器存活时间
        if let Ok(val) = std::env::var("RCODER_CONTAINER_TTL") {
            info!("应用环境变量 RCODER_CONTAINER_TTL");
            match val.parse() {
                Ok(seconds) => self.container_ttl_seconds = Some(seconds),
                Err(e) => {
                    tracing::warn!(
                        "⚠️ [CONFIG] 无法解析 RCODER_CONTAINER_TTL '{}': {}, 使用默认值",
                        val,
                        e
                    );
                }
            }
        }

        // 🔧 应用 API 超时配置
        if let Ok(val) = std::env::var("RCODER_API_TIMEOUT_SECONDS") {
            info!("应用环境变量 RCODER_API_TIMEOUT_SECONDS");
            match val.parse() {
                Ok(seconds) => self.api_timeout_seconds = Some(seconds),
                Err(e) => {
                    tracing::warn!(
                        "⚠️ [CONFIG] 无法解析 RCODER_API_TIMEOUT_SECONDS '{}': {}, 使用默认值",
                        val,
                        e
                    );
                }
            }
        }

        // 🔧 应用快速操作超时配置
        if let Ok(val) = std::env::var("RCODER_API_TIMEOUT_QUICK_SECONDS") {
            info!("应用环境变量 RCODER_API_TIMEOUT_QUICK_SECONDS");
            match val.parse() {
                Ok(seconds) => self.api_timeout_quick_seconds = Some(seconds),
                Err(e) => {
                    tracing::warn!(
                        "⚠️ [CONFIG] 无法解析 RCODER_API_TIMEOUT_QUICK_SECONDS '{}': {}, 使用默认值",
                        val,
                        e
                    );
                }
            }
        }

        // 🔧 应用状态缓存 TTL 配置
        if let Ok(val) = std::env::var("RCODER_CACHE_STATUS_TTL_SECONDS") {
            info!("应用环境变量 RCODER_CACHE_STATUS_TTL_SECONDS");
            match val.parse() {
                Ok(seconds) => self.cache_status_ttl_seconds = Some(seconds),
                Err(e) => {
                    tracing::warn!(
                        "⚠️ [CONFIG] 无法解析 RCODER_CACHE_STATUS_TTL_SECONDS '{}': {}, 使用默认值",
                        val,
                        e
                    );
                }
            }
        }

        // 🔧 应用网络缓存 TTL 配置
        if let Ok(val) = std::env::var("RCODER_CACHE_NETWORK_TTL_SECONDS") {
            info!("应用环境变量 RCODER_CACHE_NETWORK_TTL_SECONDS");
            match val.parse() {
                Ok(seconds) => self.cache_network_ttl_seconds = Some(seconds),
                Err(e) => {
                    tracing::warn!(
                        "⚠️ [CONFIG] 无法解析 RCODER_CACHE_NETWORK_TTL_SECONDS '{}': {}, 使用默认值",
                        val,
                        e
                    );
                }
            }
        }

        Ok(())
    }

    /// 获取配置摘要信息
    pub fn get_summary(&self) -> String {
        format!(
            "Docker配置: 网络模式={}, 网络基础名称={}, 工作目录={}, 自动清理={}, 容器TTL={}, API超时={}秒, 快速超时={}秒, 状态缓存={}秒, 网络缓存={}秒",
            self.network_mode.as_deref().unwrap_or("默认"),
            self.network_base_name.as_deref().unwrap_or("agent-network"),
            self.work_dir.as_deref().unwrap_or("/app"),
            self.auto_cleanup.unwrap_or(true),
            self.container_ttl_seconds.unwrap_or(3600),
            self.api_timeout_seconds.unwrap_or(10),
            self.api_timeout_quick_seconds.unwrap_or(5),
            self.cache_status_ttl_seconds.unwrap_or(10),
            self.cache_network_ttl_seconds.unwrap_or(15)
        )
    }
}

/// 加载配置（命令行参数 + 配置文件 + 环境变量）
pub fn load_config_with_args(cli_args: CliArgs) -> anyhow::Result<AppConfig> {
    let mut config = if std::path::Path::new(CONFIG_FILE).exists() {
        // 尝试从文件加载配置
        match load_config_from_file() {
            Ok(file_config) => {
                info!("已从配置文件加载: {}", CONFIG_FILE);
                file_config
            }
            Err(e) => {
                warn!("加载配置文件失败，使用默认配置: {}", e);
                AppConfig::default()
            }
        }
    } else {
        info!("配置文件不存在，创建默认配置文件: {}", CONFIG_FILE);
        let default_config = AppConfig::default();
        create_default_config_file(&default_config)?;
        default_config
    };

    // 命令行参数覆盖配置文件
    if let Some(port) = cli_args.port {
        config.port = port;
    }

    if let Some(projects_dir) = cli_args.projects_dir {
        config.projects_dir = PathBuf::from(projects_dir);
    }

    // 环境变量覆盖所有配置
    if let Ok(port) = std::env::var("RCODER_PORT") {
        if let Ok(port) = port.parse::<u16>() {
            config.port = port;
        } else {
            warn!("无效的 RCODER_PORT 环境变量值: {}", port);
        }
    }

    if let Ok(projects_dir) = std::env::var("RCODER_PROJECTS_DIR") {
        config.projects_dir = PathBuf::from(projects_dir);
    }

    // 如果启用了代理，配置代理相关参数
    if cli_args.enable_proxy {
        let mut proxy_config = ProxyConfig::default();

        if let Some(proxy_port) = cli_args.proxy_port {
            proxy_config.listen_port = proxy_port;
        }

        if let Some(default_backend_port) = cli_args.default_backend_port {
            proxy_config.default_backend_port = default_backend_port;
        }

        config.proxy_config = Some(proxy_config);
    }

    // 应用 Docker 配置的环境变量覆盖
    if let Some(docker_config) = &mut config.docker_config {
        docker_config.apply_env_overrides()?;
    }

    // 应用 API Key 配置的环境变量覆盖
    if let Ok(val) = std::env::var("RCODER_API_KEY_ENABLED") {
        if let Ok(enabled) = val.parse::<bool>() {
            config.api_key_auth.enabled = enabled;
            info!("应用环境变量 RCODER_API_KEY_ENABLED: {}", enabled);
        } else {
            warn!("无效的 RCODER_API_KEY_ENABLED 环境变量值: {}", val);
        }
    }

    if let Ok(val) = std::env::var("RCODER_API_KEY") {
        config.api_key_auth.api_key = val.clone();
        info!("应用环境变量 RCODER_API_KEY");
    }

    // 验证 API Key 配置
    if config.api_key_auth.enabled && config.api_key_auth.api_key.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "API Key 鉴权已启用但 API Key 为空,请检查配置文件或环境变量"
        ));
    }

    // 配置验证
    if let Some(docker_config) = &config.docker_config {
        if let Err(e) = docker_config.validate_multi_image_config() {
            return Err(anyhow::anyhow!("Docker 配置验证失败: {}", e));
        }
    }

    info!(
        "最终配置: port={}, projects_dir={:?}, default_agent_id={}, proxy_enabled={}",
        config.port,
        config.projects_dir,
        config.default_agent_id,
        config.proxy_config.is_some()
    );

    Ok(config)
}

/// 加载配置（保留旧接口以保持兼容性）
pub fn load_config() -> anyhow::Result<AppConfig> {
    let cli_args = CliArgs {
        port: None,
        projects_dir: None,
        enable_proxy: false,
        proxy_port: None,
        default_backend_port: None,
    };
    load_config_with_args(cli_args)
}

/// 从文件加载配置
fn load_config_from_file() -> anyhow::Result<AppConfig> {
    let config_content =
        fs::read_to_string(CONFIG_FILE).map_err(|e| anyhow::anyhow!("读取配置文件失败: {}", e))?;

    tracing::debug!("配置文件内容: {}", config_content);

    let config: AppConfig = serde_yaml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("解析配置文件失败: {}", e))?;

    // 调试：打印解析后的多镜像配置
    if let Some(ref docker_config) = config.docker_config {
        if let Some(ref multi_config) = docker_config.multi_image_config {
            tracing::debug!("解析后的多镜像配置:");
            for (service_key, service_config) in &multi_config.services {
                tracing::debug!(
                    "  服务 '{}' 挂载配置 (共 {} 个):",
                    service_key,
                    service_config.mounts.len()
                );
                for (i, mount) in service_config.mounts.iter().enumerate() {
                    tracing::debug!(
                        "    [{}]: {} -> {} ({})",
                        i,
                        mount.container_path,
                        mount.host_path,
                        mount.mount_type
                    );
                }
            }
        }
    }

    Ok(config)
}

/// 从配置文件中仅加载 API Key 配置（用于热更新）
///
/// 此函数由 config_watcher 模块调用,用于配置热重载。
/// 编译器可能误报为未使用,因为是跨模块调用。
#[allow(dead_code)]
pub fn load_api_key_config_from_file(
    config_path: &std::path::Path,
) -> anyhow::Result<ApiKeyAuthConfig> {
    let config_content =
        fs::read_to_string(config_path).map_err(|e| anyhow::anyhow!("读取配置文件失败: {}", e))?;

    let config: AppConfig = serde_yaml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("解析配置文件失败: {}", e))?;

    Ok(config.api_key_auth)
}

/// 创建默认配置文件
fn create_default_config_file(_config: &AppConfig) -> anyhow::Result<()> {
    // 检查配置文件是否已存在
    if std::path::Path::new(CONFIG_FILE).exists() {
        return Ok(());
    }

    // 创建配置文件目录（如果不存在）
    if let Some(parent) = std::path::Path::new(CONFIG_FILE).parent() {
        std::fs::create_dir_all(parent).map_err(|e| anyhow::anyhow!("创建配置目录失败: {}", e))?;
    }

    // 使用嵌入式配置文件
    let default_config = include_str!("rcoder_default.yml");

    // 🆕 生成随机 API Key 并替换模板占位符
    let generated_api_key = generate_random_api_key();
    let config_content = default_config.replace("{{GENERATED_API_KEY}}", &generated_api_key);

    fs::write(CONFIG_FILE, config_content)
        .map_err(|e| anyhow::anyhow!("写入默认配置文件失败: {}", e))?;

    info!("已创建默认配置文件: {}", CONFIG_FILE);
    info!("🔑 已生成随机 API Key（当前未启用鉴权）");
    Ok(())
}
