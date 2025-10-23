use std::env;
use std::fs;
use std::path::PathBuf;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::AgentType;

/// 命令行参数
#[derive(Parser, Debug)]
#[command(name = "rcoder")]
#[command(about = "AI-powered development platform")]
#[command(version)]
pub struct CliArgs {
    /// 服务端口
    #[arg(short, long, help = "服务端口")]
    pub port: Option<u16>,

    /// 项目工作目录
    #[arg(short = 'd', long, help = "项目工作的根目录")]
    pub projects_dir: Option<PathBuf>,

    /// 启用反向代理
    #[arg(long, help = "启用基于端口的反向代理")]
    pub enable_proxy: bool,

    /// 代理监听端口
    #[arg(long, help = "代理服务监听端口")]
    pub proxy_port: Option<u16>,

    /// 默认后端端口
    #[arg(long, help = "默认后端服务端口")]
    pub default_backend_port: Option<u16>,
}

/// 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 默认使用的 AI 代理类型
    pub default_agent: AgentType,
    /// 项目工作的根目录,根据启动命令的当前目录来确定
    pub projects_dir: PathBuf,
    /// 服务端口
    pub port: u16,
    /// 代理配置
    pub proxy_config: Option<ProxyConfig>,
    /// Docker 配置
    pub docker_config: Option<DockerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    pub enabled: bool,
    pub interval_seconds: u64,
    pub timeout_seconds: u64,
    pub healthy_threshold: u32,
    pub unhealthy_threshold: u32,
}

/// 代理配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// 代理监听端口
    pub listen_port: u16,
    /// 默认后端端口
    pub default_backend_port: u16,
    /// 后端服务主机
    pub backend_host: String,
    /// URL 中端口参数的名称
    pub port_param: String,
    /// 健康检查配置
    pub health_check: HealthCheckConfig,
}

/// Docker 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerConfig {
    /// Docker 镜像名称（根据架构自动选择）
    /// 如果不指定，将使用默认的 registry.yichamao.com/rcoder:latest
    pub image: Option<String>,
    /// ARM64 架构的 Docker 镜像
    pub arm64_image: Option<String>,
    /// AMD64 架构的 Docker 镜像
    pub amd64_image: Option<String>,
    /// 默认网络模式
    pub network_mode: Option<String>,
    /// 默认工作目录
    pub work_dir: Option<String>,
    /// 是否启用自动清理
    pub auto_cleanup: Option<bool>,
    /// 容器存活时间（秒）
    pub container_ttl_seconds: Option<u64>,
}

/// 配置文件路径
const CONFIG_FILE: &str = "config.yml";

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_agent: AgentType::Codex,
            projects_dir: PathBuf::from("./project_workspace"),
            port: 8086,
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
            health_check: HealthCheckConfig {
                enabled: true,
                interval_seconds: 5,
                timeout_seconds: 1,
                healthy_threshold: 2,
                unhealthy_threshold: 3,
            },
        }
    }
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            image: None, // 使用默认镜像
            arm64_image: Some("registry.yichamao.com/rcoder:latest-arm64".to_string()),
            amd64_image: Some("registry.yichamao.com/rcoder:latest-amd64".to_string()),
            network_mode: Some("bridge".to_string()),
            work_dir: Some("/app".to_string()),
            auto_cleanup: Some(true),
            container_ttl_seconds: Some(3600), // 1小时
        }
    }
}

// 为 DockerConfig 实现 DockerConfigTrait，以便与 docker_manager 兼容
impl docker_manager::utils::DockerConfigTrait for DockerConfig {
    fn image(&self) -> &Option<String> {
        &self.image
    }
    
    fn arm64_image(&self) -> &Option<String> {
        &self.arm64_image
    }
    
    fn amd64_image(&self) -> &Option<String> {
        &self.amd64_image
    }
    
    fn network_mode(&self) -> &Option<String> {
        &self.network_mode
    }
    
    fn work_dir(&self) -> &Option<String> {
        &self.work_dir
    }
    
    fn auto_cleanup(&self) -> &Option<bool> {
        &self.auto_cleanup
    }
    
    fn container_ttl_seconds(&self) -> &Option<u64> {
        &self.container_ttl_seconds
    }
}

/// 加载配置
/// 配置优先级：命令行参数 > 环境变量 > 配置文件 > 默认配置
pub fn load_config_with_args(cli_args: CliArgs) -> AppConfig {
    // 1. 首先加载默认配置
    let mut config = AppConfig::default();

    // 2. 尝试从当前目录读取配置文件
    match load_config_from_file() {
        Ok(file_config) => {
            config = file_config;
            info!("成功从 {} 加载配置", CONFIG_FILE);
        }
        Err(e) => {
            warn!("无法读取配置文件 {}: {}, 使用默认配置", CONFIG_FILE, e);

            // 创建默认配置文件
            if let Err(create_err) = create_default_config_file(&config) {
                error!("创建默认配置文件失败: {}", create_err);
            } else {
                info!("已创建默认配置文件: {}", CONFIG_FILE);
            }
        }
    }

    // 3. 环境变量覆盖配置
    if let Ok(port) = env::var("RCODER_PORT") {
        match port.parse::<u16>() {
            Ok(p) => {
                config.port = p;
                info!("使用环境变量 RCODER_PORT 设置端口: {}", p);
            }
            Err(_) => {
                warn!(
                    "环境变量 RCODER_PORT 值无效: {}, 使用配置文件中的端口: {}",
                    port, config.port
                );
            }
        }
    }

    // Docker 镜像环境变量支持
    if let Ok(docker_image) = env::var("RCODER_DOCKER_IMAGE") {
        if config.docker_config.is_none() {
            config.docker_config = Some(DockerConfig::default());
        }
        config.docker_config.as_mut().unwrap().image = Some(docker_image);
        info!("使用环境变量 RCODER_DOCKER_IMAGE 设置 Docker 镜像");
    }

    if let Ok(docker_arm64_image) = env::var("RCODER_DOCKER_IMAGE_ARM64") {
        if config.docker_config.is_none() {
            config.docker_config = Some(DockerConfig::default());
        }
        config.docker_config.as_mut().unwrap().arm64_image = Some(docker_arm64_image);
        info!("使用环境变量 RCODER_DOCKER_IMAGE_ARM64 设置 ARM64 镜像");
    }

    if let Ok(docker_amd64_image) = env::var("RCODER_DOCKER_IMAGE_AMD64") {
        if config.docker_config.is_none() {
            config.docker_config = Some(DockerConfig::default());
        }
        config.docker_config.as_mut().unwrap().amd64_image = Some(docker_amd64_image);
        info!("使用环境变量 RCODER_DOCKER_IMAGE_AMD64 设置 AMD64 镜像");
    }

    // 4. 处理代理配置
    if cli_args.enable_proxy {
        let proxy_config = ProxyConfig {
            listen_port: cli_args.proxy_port.unwrap_or(8080),
            default_backend_port: cli_args.default_backend_port.unwrap_or(config.port),
            backend_host: "127.0.0.1".to_string(),
            port_param: "port".to_string(),
            health_check: HealthCheckConfig {
                enabled: true,
                interval_seconds: 5,
                timeout_seconds: 1,
                healthy_threshold: 2,
                unhealthy_threshold: 3,
            },
        };
        config.proxy_config = Some(proxy_config);
        info!(
            "启用反向代理，监听端口: {}",
            config.proxy_config.as_ref().unwrap().listen_port
        );
    }

    // 5. 命令行参数覆盖配置（优先级最高）
    if let Some(port) = cli_args.port {
        config.port = port;
        info!("使用命令行参数设置端口: {}", port);
    }

    if let Some(projects_dir) = cli_args.projects_dir {
        config.projects_dir = projects_dir.clone();
        info!("使用命令行参数设置项目目录: {:?}", projects_dir);
    }

    info!(
        "最终配置: port={}, projects_dir={:?}, default_agent={:?}, proxy_enabled={}",
        config.port,
        config.projects_dir,
        config.default_agent,
        config.proxy_config.is_some()
    );

    config
}

/// 加载配置（保留旧接口以保持兼容性）
pub fn load_config() -> AppConfig {
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

    let config: AppConfig = serde_yaml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("解析配置文件失败: {}", e))?;

    Ok(config)
}

/// 创建默认配置文件
fn create_default_config_file(config: &AppConfig) -> anyhow::Result<()> {
    // 手动构建带注释的 YAML 内容
    let content_with_comments = format!(
        r#"# rcoder 配置文件
# 该文件在首次启动时自动生成

# 默认使用的 AI 代理类型 (Codex/Claude/Proxy)
default_agent: {}

# 项目工作目录
projects_dir: {}

# 主服务端口
port: {}

# Pingora 反向代理配置
proxy_config:
  # 代理服务监听端口 (用于接收外部请求)
  listen_port: {}
  # 默认后端服务端口 (当请求未指定端口时使用)
  default_backend_port: {}
  # 后端服务主机地址
  backend_host: "{}"
  # URL 中端口参数的名称 (用于从路径中提取端口号)
  port_param: "{}"
  # 健康检查配置
  health_check:
    enabled: {}
    interval_seconds: {}
    timeout_seconds: {}
    healthy_threshold: {}
    unhealthy_threshold: {}

# Docker 配置
docker_config:
  # Docker 镜像名称 (留空使用默认镜像)
  # 如果指定了此字段，将优先使用该镜像，忽略架构特定镜像
  image: {}

  # ARM64 架构专用镜像
  arm64_image: {}

  # AMD64 架构专用镜像
  amd64_image: {}

  # 默认网络模式
  network_mode: {}

  # 默认工作目录
  work_dir: {}

  # 是否启用自动清理
  auto_cleanup: {}

  # 容器存活时间（秒）
  container_ttl_seconds: {}
"#,
        format!("{:?}", config.default_agent),
        config.projects_dir.display(),
        config.port,
        config.proxy_config.as_ref().unwrap().listen_port,
        config.proxy_config.as_ref().unwrap().default_backend_port,
        config.proxy_config.as_ref().unwrap().backend_host,
        config.proxy_config.as_ref().unwrap().port_param,
        config.proxy_config.as_ref().unwrap().health_check.enabled,
        config
            .proxy_config
            .as_ref()
            .unwrap()
            .health_check
            .interval_seconds,
        config
            .proxy_config
            .as_ref()
            .unwrap()
            .health_check
            .timeout_seconds,
        config
            .proxy_config
            .as_ref()
            .unwrap()
            .health_check
            .healthy_threshold,
        config
            .proxy_config
            .as_ref()
            .unwrap()
            .health_check
            .unhealthy_threshold,
        // Docker 配置部分
        config.docker_config.as_ref().unwrap().image.as_ref().map_or("null".to_string(), |s| format!("\"{}\"", s)),
        config.docker_config.as_ref().unwrap().arm64_image.as_ref().map_or("null".to_string(), |s| format!("\"{}\"", s)),
        config.docker_config.as_ref().unwrap().amd64_image.as_ref().map_or("null".to_string(), |s| format!("\"{}\"", s)),
        config.docker_config.as_ref().unwrap().network_mode.as_ref().map_or("null".to_string(), |s| format!("\"{}\"", s)),
        config.docker_config.as_ref().unwrap().work_dir.as_ref().map_or("null".to_string(), |s| format!("\"{}\"", s)),
        config.docker_config.as_ref().unwrap().auto_cleanup.map_or("null".to_string(), |b| b.to_string()),
        config.docker_config.as_ref().unwrap().container_ttl_seconds.map_or("null".to_string(), |s| s.to_string())
    );

    fs::write(CONFIG_FILE, content_with_comments)
        .map_err(|e| anyhow::anyhow!("写入配置文件失败: {}", e))?;

    Ok(())
}
