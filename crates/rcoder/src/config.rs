use std::path::PathBuf;
use std::fs;
use std::env;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{info, warn, error};

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
}

/// 配置文件路径
const CONFIG_FILE: &str = "config.yml";

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_agent: AgentType::Codex,
            projects_dir: PathBuf::from("./project_workspace"),
            port: 3000,
        }
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
                warn!("环境变量 RCODER_PORT 值无效: {}, 使用配置文件中的端口: {}", port, config.port);
            }
        }
    }

    // 4. 命令行参数覆盖配置（优先级最高）
    if let Some(port) = cli_args.port {
        config.port = port;
        info!("使用命令行参数设置端口: {}", port);
    }
    
    if let Some(projects_dir) = cli_args.projects_dir {
        config.projects_dir = projects_dir.clone();
        info!("使用命令行参数设置项目目录: {:?}", projects_dir);
    }

    info!(
        "最终配置: port={}, projects_dir={:?}, default_agent={:?}",
        config.port, config.projects_dir, config.default_agent
    );

    config
}

/// 加载配置（保留旧接口以保持兼容性）
pub fn load_config() -> AppConfig {
    let cli_args = CliArgs {
        port: None,
        projects_dir: None,
    };
    load_config_with_args(cli_args)
}

/// 从文件加载配置
fn load_config_from_file() -> anyhow::Result<AppConfig> {
    let config_content = fs::read_to_string(CONFIG_FILE)
        .map_err(|e| anyhow::anyhow!("读取配置文件失败: {}", e))?;
    
    let config: AppConfig = serde_yaml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("解析配置文件失败: {}", e))?;
    
    Ok(config)
}

/// 创建默认配置文件
fn create_default_config_file(config: &AppConfig) -> anyhow::Result<()> {
    let yaml_content = serde_yaml::to_string(config)
        .map_err(|e| anyhow::anyhow!("序列化配置失败: {}", e))?;
    
    // 添加注释
    let content_with_comments = format!(
        "# rcoder 配置文件\n# 该文件在首次启动时自动生成\n\n{}",
        yaml_content
    );
    
    fs::write(CONFIG_FILE, content_with_comments)
        .map_err(|e| anyhow::anyhow!("写入配置文件失败: {}", e))?;
    
    Ok(())
}
