//! Auto-Reload 热重载机制（简化版）
//!
//! 当 `auto_reload.enabled=true` 时，每次请求都重启 ACP agent 进程，
//! 并尝试通过 session_id 恢复历史上下文。
//!
//! 适用场景：`/devcomputer/chat` 接口用于调试 ACP agent 功能，
//! 每次请求都应使用最新的 agent 代码。
//!
//! # 工作原理
//!
//! 1. 检查 `auto_reload.enabled` 是否为 true
//! 2. 如果启用，强制重启 ACP agent 进程
//! 3. 传入 `resume_session_id` 尝试恢复历史上下文
//! 4. 如果恢复成功，继续对话（session_id 不变）
//! 5. 如果恢复失败，创建新会话（session_id 变化）

use std::path::{Path, PathBuf};

use shared_types::AgentBinarySnapshot;
use shared_types::AutoReloadConfig;

/// 检查是否需要重启
///
/// 简化逻辑：只要 `enabled=true`，就返回 true。
///
/// # Arguments
/// * `config` - auto-reload 配置
///
/// # Returns
/// 如果需要重启返回 true，否则返回 false
#[allow(dead_code)]
pub fn should_reload(config: &AutoReloadConfig) -> bool {
    config.enabled
}

/// 解析 agent 命令对应的二进制文件路径
///
/// 对于绝对/相对路径直接返回；对于 PATH 中的命令使用 `which` 解析。
/// 用于日志记录和调试。
///
/// # Arguments
/// * `command` - agent 启动命令（如 "codex-acp", "./my-agent"）
/// * `working_dir` - 工作目录，用于解析相对路径
///
/// # Returns
/// 解析后的绝对路径，如果找不到则返回 None
#[allow(dead_code)]
pub fn resolve_agent_binary(command: &str, working_dir: &Path) -> Option<PathBuf> {
    let path = Path::new(command);

    // 如果是绝对路径，直接返回
    if path.is_absolute() {
        return if path.exists() {
            Some(path.to_path_buf())
        } else {
            None
        };
    }

    // 如果包含路径分隔符（./my-agent 或 ../my-agent），相对于 working_dir 解析
    if command.contains('/') || command.contains('\\') {
        let resolved = working_dir.join(path);
        return if resolved.exists() {
            Some(resolved)
        } else {
            None
        };
    }

    // 否则在 PATH 中查找
    which::which(command).ok()
}

/// 创建二进制文件快照
///
/// 读取文件的 metadata（mtime + size），用于日志记录和调试。
#[allow(dead_code)]
pub fn take_snapshot(path: &Path) -> Option<AgentBinarySnapshot> {
    AgentBinarySnapshot::from_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_should_reload_enabled() {
        let config = AutoReloadConfig::default_enabled();
        assert!(should_reload(&config));
    }

    #[test]
    fn test_should_reload_disabled() {
        let config = AutoReloadConfig::disabled();
        assert!(!should_reload(&config));
    }

    #[test]
    fn test_resolve_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("my-agent");
        fs::write(&bin_path, "fake binary").unwrap();

        let result = resolve_agent_binary(bin_path.to_str().unwrap(), tmp.path());
        assert_eq!(result, Some(bin_path));
    }

    #[test]
    fn test_resolve_relative_path() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("my-agent");
        fs::write(&bin_path, "fake binary").unwrap();

        let result = resolve_agent_binary("./my-agent", tmp.path());
        assert_eq!(result, Some(bin_path));
    }

    #[test]
    fn test_resolve_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_agent_binary("./nonexistent-agent", tmp.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_take_snapshot() {
        let tmp = TempDir::new().unwrap();
        let bin_path = tmp.path().join("my-agent");
        fs::write(&bin_path, "version 1").unwrap();

        let snap = take_snapshot(&bin_path);
        assert!(snap.is_some());

        let snap = snap.unwrap();
        assert_eq!(snap.path, bin_path);
        assert!(snap.size_bytes > 0);
    }
}
