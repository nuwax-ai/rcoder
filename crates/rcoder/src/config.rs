use std::path::PathBuf;

use tracing::info;

use crate::AgentType;

/// 应用配置
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// 默认使用的 AI 代理类型
    pub default_agent: AgentType,
    /// 项目工作目录
    pub projects_dir: PathBuf,
    /// 服务端口
    pub port: u16,
}

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
pub fn load_config() -> AppConfig {
    let mut config = AppConfig::default();

    if let Ok(port) = std::env::var("PORT") {
        config.port = port.parse().unwrap_or(3000);
    }

    if let Ok(projects_dir) = std::env::var("PROJECTS_DIR") {
        config.projects_dir = PathBuf::from(projects_dir);
    }

    info!(
        "Loaded config: port={}, projects_dir={:?}",
        config.port, config.projects_dir
    );

    config
}
