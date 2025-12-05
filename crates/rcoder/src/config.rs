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
}

fn default_agent_id() -> String {
    "claude-code-acp".to_string()
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
}

const CONFIG_FILE: &str = "config.yml";

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_agent_id: default_agent_id(),
            projects_dir: PathBuf::from("./project_workspace"),
            port: 8087,
            proxy_config: Some(ProxyConfig::default()),
            docker_config: Some(DockerConfig::default()),
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
            self.container_ttl_seconds = val.parse().ok();
        }

        Ok(())
    }

    /// 获取配置摘要信息
    pub fn get_summary(&self) -> String {
        format!(
            "Docker配置: 网络模式={}, 工作目录={}, 自动清理={}, 容器TTL={}",
            self.network_mode.as_deref().unwrap_or("默认"),
            self.work_dir.as_deref().unwrap_or("/app"),
            self.auto_cleanup.unwrap_or(true),
            self.container_ttl_seconds.unwrap_or(3600)
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

    fs::write(CONFIG_FILE, default_config)
        .map_err(|e| anyhow::anyhow!("写入默认配置文件失败: {}", e))?;

    info!("已创建默认配置文件: {}", CONFIG_FILE);
    Ok(())
}
