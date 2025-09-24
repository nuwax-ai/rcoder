use std::path::PathBuf;

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
