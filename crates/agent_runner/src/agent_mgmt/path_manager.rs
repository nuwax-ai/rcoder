//! 安装目录与 PATH 注入 (P0-1)
//!
//! 默认布局(可在 `PathManager::new_with_root` 覆盖):
//! ```text
//! {install_dir}/
//! ├── registry.json               # 已安装 agent 注册表(持久化)
//! ├── claude-code-acp/            # builtin agent 目录(只读,不可卸载)
//! │   └── ...
//! ├── codex-acp/                  # 二进制型 agent
//! │   ├── codex-acp               # 入口可执行文件 (binary_path)
//! │   └── ...
//! └── deepagents-app-agent/       # 目录型 agent (Node.js / Bun / Python)
//!     ├── dist/index.js            # 入口脚本 (bin.start)
//!     ├── node_modules/
//!     └── agent-package.json
//! ```
//!
//! ## PATH 策略
//!
//! - Dockerfile 将 `{install_dir}` 加入 PATH
//! - start-up.sh 动态注入子目录到 PATH（使二进制型 agent 可被 `which` 找到）
//! - 目录型 agent 的 command 是解释器（node / bun / python），已在系统 PATH 中

use std::path::{Path, PathBuf};

use shared_types::DEFAULT_ACP_AGENT_INSTALL_DIR;

/// Agent 安装目录与 PATH 管理
#[derive(Debug, Clone)]
pub struct PathManager {
    /// 安装根目录(默认 $HOME/acp-agent,容器内 $HOME 通常是 /home/user 或 /root)
    install_dir: PathBuf,
}

impl Default for PathManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PathManager {
    /// 使用默认安装目录($HOME/acp-agent)
    pub fn new() -> Self {
        let install_dir = dirs_home().join("acp-agent");
        Self { install_dir }
    }

    /// 使用自定义根目录(测试用)
    #[allow(dead_code)] // test-only constructor
    pub fn new_with_root(root: PathBuf) -> Self {
        Self { install_dir: root }
    }

    /// 安装根目录
    pub fn install_dir(&self) -> &Path {
        &self.install_dir
    }

    /// bin 子目录(写入 PATH)
    pub fn bin_dir(&self) -> PathBuf {
        self.install_dir.join("bin")
    }

    /// 注册表文件路径
    pub fn registry_path(&self) -> PathBuf {
        self.install_dir.join("registry.json")
    }

    /// 单个 agent 的隔离目录
    ///
    /// **注意**:若 `agent_id` 非法,返回 `Err(String)`,由调用方决定是 `?` 上抛还是降级。
    /// 不再使用 `.expect()` 以满足 "Fail Fast 但不 panic" 原则。
    pub fn agent_dir(&self, agent_id: &str) -> Result<PathBuf, String> {
        validate_agent_id(agent_id)?;
        Ok(self.install_dir.join(agent_id))
    }

    /// 确保 bin 目录存在(惰性创建)
    pub async fn ensure_dirs(&self) -> Result<(), std::io::Error> {
        tokio::fs::create_dir_all(&self.install_dir).await?;
        tokio::fs::create_dir_all(self.bin_dir()).await?;
        Ok(())
    }
}

/// 校验 agent_id 合法性(防路径遍历)
/// 仅允许 ASCII 字母/数字/`-`/`_`/`.`,长度 1-128
pub fn validate_agent_id(agent_id: &str) -> Result<(), String> {
    if agent_id.is_empty() || agent_id.len() > 128 {
        return Err("agent_id length must be 1-128".to_string());
    }
    if !agent_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err("agent_id may only contain [A-Za-z0-9._-]".to_string());
    }
    if agent_id.starts_with('.') || agent_id.contains("..") {
        return Err("agent_id may not start with '.' or contain '..'".to_string());
    }
    Ok(())
}

/// 校验 command 参数合法性(防路径遍历)
///
/// command 作为最终写入 `bin_dir` 的文件名,禁止路径分隔符和遍历序列。
/// 允许 ASCII 字母/数字/`-`/`_`/`.`，长度 1-256。
pub fn validate_command(command: &str) -> Result<(), String> {
    if command.is_empty() || command.len() > 256 {
        return Err("command length must be 1-256".to_string());
    }
    if command.contains('/') || command.contains('\\') {
        return Err("command may not contain '/' or '\\'".to_string());
    }
    if command.contains("..") {
        return Err("command may not contain '..'".to_string());
    }
    if !command
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err("command may only contain [A-Za-z0-9._-]".to_string());
    }
    if command.starts_with('.') {
        return Err("command may not start with '.'".to_string());
    }
    Ok(())
}

fn dirs_home() -> PathBuf {
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty() {
            return PathBuf::from(home);
        }
    // Windows fallback
    if let Ok(profile) = std::env::var("USERPROFILE")
        && !profile.is_empty() {
            return PathBuf::from(profile);
        }
    PathBuf::from(DEFAULT_ACP_AGENT_INSTALL_DIR)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_safe_agent_ids() {
        assert!(validate_agent_id("claude-code-acp").is_ok());
        assert!(validate_agent_id("kimi_cli").is_ok());
        assert!(validate_agent_id("codex-acp.v1").is_ok());
        assert!(validate_agent_id("a").is_ok());
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(validate_agent_id("../etc").is_err());
        assert!(validate_agent_id("foo/bar").is_err());
        assert!(validate_agent_id("foo\\bar").is_err());
        assert!(validate_agent_id("..").is_err());
        assert!(validate_agent_id(".hidden").is_err());
    }

    #[test]
    fn rejects_empty_and_oversized() {
        assert!(validate_agent_id("").is_err());
        let long: String = "a".repeat(129);
        assert!(validate_agent_id(&long).is_err());
    }

    #[test]
    fn rejects_non_ascii() {
        assert!(validate_agent_id("中文-agent").is_err());
        assert!(validate_agent_id("agent🚀").is_err());
    }

    #[test]
    fn validate_command_accepts_safe() {
        assert!(validate_command("codex-acp").is_ok());
        assert!(validate_command("claude_code").is_ok());
        assert!(validate_command("agent.v2").is_ok());
        assert!(validate_command("a").is_ok());
    }

    #[test]
    fn validate_command_rejects_path_traversal() {
        assert!(validate_command("../evil").is_err());
        assert!(validate_command("foo/bar").is_err());
        assert!(validate_command("foo\\bar").is_err());
        assert!(validate_command("..").is_err());
        assert!(validate_command(".hidden").is_err());
        assert!(validate_command("/absolute/path").is_err());
    }

    #[test]
    fn validate_command_rejects_empty_and_oversized() {
        assert!(validate_command("").is_err());
        let long: String = "a".repeat(257);
        assert!(validate_command(&long).is_err());
    }

    #[test]
    fn validate_command_rejects_non_ascii() {
        assert!(validate_command("命令").is_err());
    }

    #[test]
    fn path_manager_uses_custom_root() {
        let pm = PathManager::new_with_root(PathBuf::from("/tmp/test-acp"));
        assert_eq!(pm.install_dir(), Path::new("/tmp/test-acp"));
        assert_eq!(pm.bin_dir(), PathBuf::from("/tmp/test-acp/bin"));
        assert_eq!(
            pm.registry_path(),
            PathBuf::from("/tmp/test-acp/registry.json")
        );
        assert_eq!(
            pm.agent_dir("codex-acp").unwrap(),
            PathBuf::from("/tmp/test-acp/codex-acp")
        );
    }
}
