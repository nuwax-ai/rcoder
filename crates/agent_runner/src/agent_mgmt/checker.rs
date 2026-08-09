//! Agent 健康/版本/可执行性检查 (P0-1)
//!
//! 三种检查:
//! - 静态检查(file_exists / executable / in_path)
//! - 版本检测(执行 `<command> --version` 并解析输出)
//! - 完整性检查(注册表条目与磁盘状态一致)
//!
//! 版本缓存:
//! - 内置 agent 版本号在启动时缓存，避免每次请求都执行子进程
//! - 非内置 agent 保持实时检测

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use shared_types::{
    AgentDetailInfo, AgentInstallStatus, BUILTIN_AGENT_IDS, InstallType, StaticCheckResult,
};
use tracing::info;

use crate::agent_mgmt::installer::AgentManifest;
use crate::agent_mgmt::path_manager::PathManager;

/// 版本缓存（启动时填充，运行期间只读）
///
/// 使用 OnceLock<HashMap>，一次性初始化后只读，无需并发安全。
static VERSION_CACHE: OnceLock<HashMap<String, String>> = OnceLock::new();

/// 初始化内置 agent 版本缓存（启动时调用）
///
/// 遍历内置 agent 列表，执行 `{command} -v` 检测版本号并缓存。
/// 此函数应在服务启动时异步调用，不阻塞主流程。
pub async fn init_builtin_agent_versions() {
    let mut cache = HashMap::new();

    for agent in BUILTIN_AGENT_IDS {
        if let Some(version) = detect_agent_version(agent).await {
            cache.insert(agent.to_string(), version.clone());
            info!("📦 [VERSION_CACHE] Cached {} version: {}", agent, version);
        }
    }

    // 一次性设置缓存
    if let Err(e) = VERSION_CACHE.set(cache) {
        tracing::warn!("VERSION_CACHE set failed (already initialized): {e:?}");
    }
}

/// 获取 agent 版本（优先使用缓存）
///
/// 对于内置 agent，直接返回缓存的版本号（启动时已检测）。
/// 对于非内置 agent，实时执行 `{command} -v` 检测。
pub async fn get_agent_version(command: &str) -> Option<String> {
    // 1. 先检查缓存
    if let Some(cache) = VERSION_CACHE.get()
        && let Some(version) = cache.get(command)
    {
        return Some(version.clone());
    }

    // 2. 缓存未命中，实时检测（非内置 agent）
    detect_agent_version(command).await
}

pub struct AgentChecker {
    path_manager: PathManager,
}

impl AgentChecker {
    pub fn new(path_manager: PathManager) -> Self {
        Self { path_manager }
    }

    /// 静态检查:文件存在 / 可执行 / bin 目录在 PATH 中
    ///
    /// 支持两种 binary_path 类型：
    /// - 目录型 agent：binary_path 是目录 → 检查 command 是否在 PATH
    /// - 二进制 agent：binary_path 是文件 → 检查存在性和可执行权限
    pub fn static_check(&self, manifest: &AgentManifest) -> StaticCheckResult {
        let path = Path::new(&manifest.binary_path);
        if path.is_dir() {
            // 目录型 agent（Node.js / Bun / Python 等）：检查 command 是否在 PATH
            let in_path = which::which(&manifest.command).is_ok();
            return StaticCheckResult {
                file_exists: true,
                executable: in_path, // command 可找到即视为可执行
                in_path,
            };
        }
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

/// 用 which crate（跨平台）检查 agent 命令是否在 PATH 中
///
/// 返回解析后的完整路径，未找到时返回错误消息（提示安装）。
pub fn check_agent_exists(command: &str) -> Result<std::path::PathBuf, String> {
    which::which(command).map_err(|_| {
        format!(
            "agent '{}' not found in PATH, please install via /agent-mgmt/agents/install-from-url",
            command
        )
    })
}

/// 执行 `{command} -v` 检测 agent 版本号（5s 超时，best-effort）
///
/// 成功返回版本字符串，失败/超时/解析失败返回 None。
pub async fn detect_agent_version(command: &str) -> Option<String> {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new(command).arg("-v").output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            parse_version(&stdout)
        }
        _ => None,
    }
}

/// 解析 "<tool> X.Y.Z" 格式
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

    #[test]
    fn check_agent_exists_finds_real_command() {
        // "sh" 应该在 PATH 中（Unix）
        #[cfg(unix)]
        {
            let result = check_agent_exists("sh");
            assert!(result.is_ok(), "sh should be in PATH");
        }
    }

    #[test]
    fn check_agent_exists_rejects_missing_command() {
        let result = check_agent_exists("definitely-nonexistent-agent-xyz");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found in PATH"));
    }

    #[tokio::test]
    async fn detect_agent_version_returns_none_for_missing() {
        let version = detect_agent_version("definitely-nonexistent-agent-xyz").await;
        assert!(version.is_none());
    }
}
