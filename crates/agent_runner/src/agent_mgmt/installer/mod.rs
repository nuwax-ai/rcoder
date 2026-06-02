//! Installer 子模块 (P0-1)
//!
//! 4 种安装方式:
//! - [`binary_installer`] — 流式接收 binary 字节(单文件/tar.gz/zip)
//! - [`npm_installer`]   — npm i -g
//! - [`url_installer`]   — HTTP/HTTPS 下载
//! - [`archive_installer`] — 共享的解压逻辑(路径遍历拦截 + zip bomb 防护)
//! - [`default_agents`]  — 启动时注册内置 agent

pub mod archive_installer;
pub mod binary_installer;
pub mod default_agents;
pub mod npm_installer;
pub mod url_installer;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use shared_types::InstallType;

use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};

/// Agent manifest(注册表内存储的条目)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentManifest {
    /// Agent ID
    pub agent_id: String,
    /// 安装类型
    pub install_type: InstallType,
    /// 入口可执行文件(可执行文件名,不含路径)
    pub command: String,
    /// 启动参数
    #[serde(default)]
    pub args: Vec<String>,
    /// 入口可执行文件的绝对路径
    pub binary_path: String,
    /// 源 URL(URL/npm 安装时)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// 版本(若可检测)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// 文件大小(字节)
    pub file_size: u64,
    /// 文件类型("executable" / "tar.gz" / "zip")
    pub file_type: String,
    /// 安装时间(Unix timestamp 秒)
    pub installed_at: i64,
}

impl AgentManifest {
    /// 创建新 manifest,自动填充 installed_at
    #[allow(dead_code)] // used in tests across multiple modules
    pub fn new(
        agent_id: String,
        install_type: InstallType,
        command: String,
        args: Vec<String>,
        binary_path: String,
        file_size: u64,
        file_type: String,
    ) -> Self {
        Self {
            agent_id,
            install_type,
            command,
            args,
            binary_path,
            source: None,
            version: None,
            file_size,
            file_type,
            installed_at: Utc::now().timestamp(),
        }
    }

    /// 校验 manifest 合法性
    pub fn validate(&self) -> AgentMgmtResult<()> {
        if self.agent_id.is_empty() {
            return Err(AgentMgmtError::InvalidManifest("agent_id is empty".into()));
        }
        if self.command.is_empty() {
            return Err(AgentMgmtError::InvalidManifest("command is empty".into()));
        }
        if self.binary_path.is_empty() {
            return Err(AgentMgmtError::InvalidManifest("binary_path is empty".into()));
        }
        crate::agent_mgmt::path_manager::validate_agent_id(&self.agent_id)
            .map_err(AgentMgmtError::InvalidManifest)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_validates_required_fields() {
        let mut m = AgentManifest::new(
            "codex-acp".into(),
            InstallType::Binary,
            "codex-acp".into(),
            vec![],
            "/home/user/acp-agent/bin/codex-acp".into(),
            1024,
            "executable".into(),
        );
        m.installed_at = 12345;
        assert!(m.validate().is_ok());

        m.agent_id = String::new();
        assert!(m.validate().is_err());
        m.agent_id = "codex-acp".into();
        m.command = String::new();
        assert!(m.validate().is_err());
    }

    #[test]
    fn manifest_rejects_path_traversal_in_agent_id() {
        let m = AgentManifest::new(
            "../etc/passwd".into(),
            InstallType::Binary,
            "passwd".into(),
            vec![],
            "/etc/passwd".into(),
            100,
            "executable".into(),
        );
        assert!(m.validate().is_err());
    }
}
