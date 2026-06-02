//! Agent 健康/版本/可执行性检查 (P0-1)
//!
//! 三种检查:
//! - 静态检查(file_exists / executable / in_path)
//! - 版本检测(执行 `<command> --version` 并解析输出)
//! - 完整性检查(注册表条目与磁盘状态一致)

use std::path::Path;

use shared_types::{AgentDetailInfo, AgentInstallStatus, InstallType, StaticCheckResult};

use crate::agent_mgmt::installer::AgentManifest;
use crate::agent_mgmt::path_manager::PathManager;

pub struct AgentChecker {
    path_manager: PathManager,
}

impl AgentChecker {
    pub fn new(path_manager: PathManager) -> Self {
        Self { path_manager }
    }

    /// 静态检查:文件存在 / 可执行 / bin 目录在 PATH 中
    pub fn static_check(&self, manifest: &AgentManifest) -> StaticCheckResult {
        let path = Path::new(&manifest.binary_path);
        let file_exists = path.exists();
        let executable = if file_exists {
            is_executable(path)
        } else {
            false
        };
        let in_path = is_in_path(&self.path_manager.bin_dir());
        StaticCheckResult {
            file_exists,
            executable,
            in_path,
        }
    }

    /// 推断详细状态
    pub fn detail_info(&self, manifest: Option<&AgentManifest>) -> AgentDetailInfo {
        match manifest {
            Some(m) => {
                let checks = self.static_check(m);
                let status = if checks.file_exists && checks.executable {
                    AgentInstallStatus::Available
                } else {
                    AgentInstallStatus::Broken
                };
                let version_check_supported = supports_version_check(&m.command);
                AgentDetailInfo {
                    agent_id: m.agent_id.clone(),
                    install_type: m.install_type,
                    installed: true,
                    status,
                    version: m.version.clone(),
                    version_check_supported,
                    static_checks: checks,
                }
            }
            None => AgentDetailInfo {
                agent_id: String::new(),
                install_type: InstallType::Binary,
                installed: false,
                status: AgentInstallStatus::NotInstalled,
                version: None,
                version_check_supported: false,
                static_checks: StaticCheckResult::default(),
            },
        }
    }

}

/// 检查文件是否可执行(Unix: 检查 mode bit;Windows: 总是 true)
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            return meta.permissions().mode() & 0o111 != 0;
        }
        false
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

/// 检查目录是否在 $PATH 中
fn is_in_path(bin_dir: &Path) -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    let bin_str = bin_dir.to_string_lossy();
    std::env::split_paths(&path_var).any(|p| p == *bin_str)
}

/// 启发式:某些 agent 不支持 --version(返回 false)
fn supports_version_check(command: &str) -> bool {
    // npm 全局安装的 agent 通常支持;codex-acp 不支持
    !matches!(command, "codex-acp" | "codex")
}

/// 解析 "<tool> X.Y.Z" 格式
#[allow(dead_code)]
fn parse_version(text: &str) -> Option<String> {
    // 常见格式: "codex-acp 1.2.3", "v1.2.3", "1.2.3", "name version 1.2.3"
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 找首个形如 v?\d+\.\d+ 的 token
        for token in line.split_whitespace() {
            let cleaned = token.trim_start_matches('v');
            if cleaned
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
                && cleaned.contains('.')
            {
                return Some(cleaned.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_basic() {
        assert_eq!(parse_version("my-tool 1.2.3"), Some("1.2.3".into()));
        assert_eq!(parse_version("v2.0.0-beta"), Some("2.0.0-beta".into()));
        assert_eq!(parse_version("name version 0.1.0"), Some("0.1.0".into()));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("hello world"), None);
    }

    #[test]
    fn supports_version_check_heuristic() {
        assert!(supports_version_check("claude-code-acp"));
        assert!(!supports_version_check("codex-acp"));
    }

    #[test]
    fn static_check_returns_false_for_missing_file() {
        let pm = PathManager::new_with_root(std::path::PathBuf::from("/tmp/agent-mgmt-test"));
        let checker = AgentChecker::new(pm);
        let m = AgentManifest::new(
            "ghost".into(),
            InstallType::Binary,
            "ghost".into(),
            vec![],
            "/nonexistent/ghost".into(),
            0,
            "executable".into(),
        );
        let checks = checker.static_check(&m);
        assert!(!checks.file_exists);
        assert!(!checks.executable);
    }
}
