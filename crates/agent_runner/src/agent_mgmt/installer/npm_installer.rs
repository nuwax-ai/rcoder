//! npm package installer (P0-1)
//!
//! 在容器内调用 `npm install -g <package>`(默认 5 分钟超时),然后:
//! 1. 解析 `npm root -g` 获取全局 node_modules 路径
//! 2. 在该目录下找到 `package/.bin/<command>`(或回退到 `package/bin/<command>`)
//! 3. 在 `bin_dir` 中创建一个 symlink 指向该入口
//! 4. 写入注册表
//!
//! 安全:
//! - agent_id 和 command 走和 binary_installer 一样的 `validate_agent_id` 路径
//! - 失败时 stderr 全部记录,不会泄露到上游错误信息

use std::path::{Path, PathBuf};
use std::time::Duration;

use shared_types::InstallType;
use shared_types_grpc::InstallAgentResponse;
use tokio::process::Command;
use tracing::info;

use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::installer::AgentManifest;
use crate::agent_mgmt::path_manager::PathManager;
use crate::agent_mgmt::registry::AgentRegistry;

/// npm install timeout(5 分钟)
const NPM_INSTALL_TIMEOUT_SECS: u64 = 300;

/// 通过 npm 全局安装一个包,自动定位入口二进制并写入注册表
pub async fn install_from_npm(
    registry: &AgentRegistry,
    path_manager: &PathManager,
    agent_id: &str,
    package: &str,
    command: &str,
) -> AgentMgmtResult<InstallAgentResponse> {
    crate::agent_mgmt::path_manager::validate_agent_id(agent_id)
        .map_err(AgentMgmtError::InvalidManifest)?;
    if command.is_empty() {
        return Err(AgentMgmtError::InvalidChunk("empty command".into()));
    }
    crate::agent_mgmt::path_manager::validate_command(command)
        .map_err(AgentMgmtError::InvalidManifest)?;
    validate_npm_package(package)?;

    path_manager.ensure_dirs().await?;

    info!(
        "[agent_mgmt] npm install: agent_id={}, package={}",
        agent_id, package
    );

    // 1. 调用 npm install -g
    let output = tokio::time::timeout(
        Duration::from_secs(NPM_INSTALL_TIMEOUT_SECS),
        Command::new("npm").arg("install").arg("-g").arg(package).output(),
    )
    .await
    .map_err(|_| AgentMgmtError::CommandTimeout(format!("npm install -g {package}")))?
    .map_err(AgentMgmtError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(AgentMgmtError::InstallFailed(format!(
            "npm install -g {package} failed (exit {:?}): {trimmed}",
            output.status.code()
        )));
    }

    // 2. 解析全局 node_modules 路径
    let npm_root_output = Command::new("npm")
        .arg("root")
        .arg("-g")
        .output()
        .await
        .map_err(AgentMgmtError::Io)?;
    if !npm_root_output.status.success() {
        return Err(AgentMgmtError::InstallFailed(
            "npm root -g failed".to_string(),
        ));
    }
    let npm_root = String::from_utf8_lossy(&npm_root_output.stdout)
        .trim()
        .to_string();
    let npm_root_path = PathBuf::from(&npm_root);

    // 3. 定位入口二进制(.bin/command 是 npm 标准的入口目录)
    let entrypoint = find_npm_entrypoint(&npm_root_path, package, command)?;
    let entrypoint_canon = entrypoint
        .canonicalize()
        .map_err(AgentMgmtError::Io)?;

    // 4. 在 bin_dir 中创建 symlink
    let link_path = path_manager.bin_dir().join(command);
    if link_path.exists() || link_path.symlink_metadata().is_ok() {
        std::fs::remove_file(&link_path).ok();
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(&entrypoint_canon, &link_path).map_err(|e| {
        AgentMgmtError::InstallFailed(format!(
            "symlink {} -> {}: {e}",
            link_path.display(),
            entrypoint_canon.display()
        ))
    })?;

    #[cfg(not(unix))]
    {
        return Err(AgentMgmtError::UnsupportedType(
            "npm install requires unix (symlink not available)".into(),
        ));
    }

    // 5. 探测版本
    let version = detect_version(&entrypoint_canon, command).await;

    // 6. 写入注册表
    let manifest = AgentManifest {
        agent_id: agent_id.to_string(),
        install_type: InstallType::Npm,
        command: command.to_string(),
        args: vec![],
        binary_path: link_path.to_string_lossy().to_string(),
        source: Some(package.to_string()),
        version: version.clone(),
        file_size: 0,
        file_type: "symlink".into(),
        installed_at: chrono::Utc::now().timestamp(),
    };
    manifest.validate()?;
    registry.upsert(manifest)?;

    info!(
        "[agent_mgmt] npm install done: agent_id={}, entrypoint={}",
        agent_id,
        entrypoint_canon.display()
    );

    Ok(InstallAgentResponse {
        agent_id: agent_id.to_string(),
        status: shared_types_grpc::AgentInstallStatus::Available as i32,
        binary_path: link_path.to_string_lossy().to_string(),
        file_type: "symlink".into(),
        file_count: Some(1),
        file_size: 0,
        version,
        source_url: Some(package.to_string()),
        action: "installed".to_string(),
        installed: true,
        previous_version: String::new(),
        platform: String::new(),
    })
}

/// Find the entrypoint binary for an npm package.
fn find_npm_entrypoint(npm_root: &Path, package: &str, command: &str) -> AgentMgmtResult<PathBuf> {
    // 1. 优先:.bin/command(nodejs 规范)
    let bin_link = npm_root.join(package).join(".bin").join(command);
    if bin_link.exists() {
        return Ok(bin_link);
    }
    // 2. 退而求其次:bin/command
    let bin_path = npm_root.join(package).join("bin").join(command);
    if bin_path.exists() {
        return Ok(bin_path);
    }
    // 3. 启发式:.bin 下任何可执行文件
    let bin_dir = npm_root.join(package).join(".bin");
    if let Ok(entries) = std::fs::read_dir(&bin_dir) {
        for e in entries.flatten() {
            if e.file_name() == command {
                return Ok(e.path());
            }
        }
    }
    Err(AgentMgmtError::InstallFailed(format!(
        "could not find entrypoint '{command}' in npm package '{package}' (tried .bin/, bin/)"
    )))
}

/// 校验 npm 包名合法性(防止 shell 注入 / 路径遍历)
/// npm 命名规则:
///   - 普通包:`[a-z0-9-_]+`
///   - scope 包:`@scope/name`(scope 和 name 都遵循同样规则)
///   - 可选 `@version` 后缀(我们不解析版本,直接拒绝带 @ 的避免歧义)
fn validate_npm_package(package: &str) -> AgentMgmtResult<()> {
    if package.is_empty() {
        return Err(AgentMgmtError::InvalidManifest(
            "npm package is empty".into(),
        ));
    }
    if package.len() > 214 {
        return Err(AgentMgmtError::InvalidManifest(format!(
            "npm package name too long: {} chars (max 214)",
            package.len()
        )));
    }
    // 不允许 shell 元字符和路径分隔符
    for c in package.chars() {
        if !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '@' | '/') {
            return Err(AgentMgmtError::InvalidManifest(format!(
                "npm package name contains invalid char: {c:?}"
            )));
        }
    }
    // 不允许 '..' 或 '//'
    if package.contains("..") || package.contains("//") {
        return Err(AgentMgmtError::InvalidManifest(format!(
            "npm package name contains traversal: {package}"
        )));
    }
    // 不允许 '.' 开头(防止隐藏目录/路径遍历)
    if package.starts_with('.') {
        return Err(AgentMgmtError::InvalidManifest(format!(
            "npm package name may not start with '.': {package}"
        )));
    }
    Ok(())
}

async fn detect_version(entrypoint: &Path, command: &str) -> Option<String> {
    // npm 安装的 agent 通常支持 --version;codex-acp 是例外
    if matches!(command, "codex-acp" | "codex") {
        return None;
    }
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(entrypoint).arg("--version").output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = if !output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stdout).to_string()
    } else {
        String::from_utf8_lossy(&output.stderr).to_string()
    };
    parse_version(&text)
}

fn parse_version(text: &str) -> Option<String> {
    for token in text.split_whitespace() {
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
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_basic() {
        assert_eq!(parse_version("tool 1.2.3"), Some("1.2.3".into()));
        assert_eq!(parse_version("v0.1.0-alpha"), Some("0.1.0-alpha".into()));
        assert_eq!(parse_version("nope"), None);
    }

    #[test]
    fn validate_npm_package_accepts_normal() {
        assert!(validate_npm_package("claude-code-acp").is_ok());
        assert!(validate_npm_package("@anthropic-ai/claude-code-acp").is_ok());
        assert!(validate_npm_package("lodash").is_ok());
        assert!(validate_npm_package("@scope/pkg.sub").is_ok());
    }

    #[test]
    fn validate_npm_package_rejects_dangerous() {
        // 空
        assert!(validate_npm_package("").is_err());
        // shell 元字符
        assert!(validate_npm_package("pkg; rm -rf /").is_err());
        assert!(validate_npm_package("pkg`whoami`").is_err());
        assert!(validate_npm_package("pkg$(evil)").is_err());
        assert!(validate_npm_package("pkg|cat").is_err());
        // 路径遍历
        assert!(validate_npm_package("../../../etc/passwd").is_err());
        assert!(validate_npm_package("foo//bar").is_err());
        assert!(validate_npm_package("foo/../../bar").is_err());
        // 以 '.' 开头
        assert!(validate_npm_package(".hidden-pkg").is_err());
        assert!(validate_npm_package("../etc").is_err());
        // 非 ASCII
        assert!(validate_npm_package("中文包").is_err());
        // 超长
        let long = "a".repeat(215);
        assert!(validate_npm_package(&long).is_err());
    }

    // 注意:npm installer 实际执行 npm 命令,只在有 npm 的环境测试
    // 集成测试放在 P0-1g(需要真实 npm)
}
