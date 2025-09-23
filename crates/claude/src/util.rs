//! Claude Code ACP 工具函数
//!
//! 基于 Zed 编辑器的实现，提供 claude-code-acp 的自动安装和管理功能。
//! 参考: https://github.com/zed-industries/claude-code-acp

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::OnceLock;
use anyhow::{Context, Result, anyhow};
use semver::Version;
use tracing::{info, debug};

/// Claude Code ACP 代理配置
#[derive(Debug, Clone)]
pub struct ClaudeCodeAcpConfig {
    /// 最小版本要求
    pub minimum_version: Option<Version>,
    /// 包名
    pub package_name: String,
    /// 入口路径
    pub entrypoint_path: PathBuf,
    /// 二进制名称
    pub binary_name: String,
    /// 自定义命令
    pub custom_command: Option<String>,
    /// 是否忽略系统版本
    pub ignore_system_version: bool,
}

impl Default for ClaudeCodeAcpConfig {
    fn default() -> Self {
        Self {
            minimum_version: Some("0.2.5".parse().unwrap_or_else(|_| "0.0.0".parse().unwrap())),
            package_name: "@zed-industries/claude-code-acp".to_string(),
            entrypoint_path: PathBuf::from("node_modules/@zed-industries/claude-code-acp/dist/index.js"),
            binary_name: "claude-code-acp".to_string(),
            custom_command: None,
            ignore_system_version: false,
        }
    }
}

/// Claude Code ACP 安装状态
#[derive(Debug, Clone)]
pub struct ClaudeCodeAcpStatus {
    /// 是否已安装
    pub is_installed: bool,
    /// 当前版本
    pub current_version: Option<Version>,
    /// 最新版本
    pub latest_version: Option<Version>,
    /// 安装路径
    pub install_path: Option<PathBuf>,
    /// 是否需要更新
    pub needs_update: bool,
    /// 状态消息
    pub status_message: String,
}

/// Claude Code ACP 命令信息
#[derive(Debug, Clone)]
pub struct ClaudeCodeAcpCommand {
    /// 命令路径
    pub path: PathBuf,
    /// 命令参数
    pub args: Vec<String>,
    /// 环境变量
    pub env: Option<std::collections::HashMap<String, String>>,
}

/// Claude Code ACP 管理器
pub struct ClaudeCodeAcpManager {
    config: ClaudeCodeAcpConfig,
    install_dir: PathBuf,
}

impl ClaudeCodeAcpManager {
    /// 创建新的管理器
    pub fn new(config: ClaudeCodeAcpConfig) -> Self {
        let install_dir = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("external_agents")
            .join(&config.binary_name);

        Self {
            config,
            install_dir,
        }
    }

    /// 使用默认配置创建管理器
    pub fn default() -> Self {
        Self::new(ClaudeCodeAcpConfig::default())
    }

    /// 获取安装状态
    pub async fn get_status(&self) -> Result<ClaudeCodeAcpStatus> {
        let mut installed_versions = Vec::new();
        let mut to_delete = Vec::new();

        // 检查安装目录
        if !self.install_dir.exists() {
            tokio::fs::create_dir_all(&self.install_dir).await
                .context("创建安装目录失败")?;
        }

        // 扫描已安装的版本
        let mut entries = tokio::fs::read_dir(&self.install_dir).await
            .context("读取安装目录失败")?;

        while let Some(entry) = entries.next_entry().await? {
            let file_name = entry.file_name();
            let Some(file_name_str) = file_name.to_str() else { continue };

            if let Ok(version) = Version::from_str(file_name_str) {
                let entry_path = entry.path().join(&self.config.entrypoint_path);
                if entry_path.exists() {
                    installed_versions.push((version, file_name_str.to_string()));
                } else {
                    to_delete.push(file_name_str.to_string());
                }
            } else {
                to_delete.push(file_name_str.to_string());
            }
        }

        // 清理无效版本
        for version_dir in to_delete {
            let dir_path = self.install_dir.join(&version_dir);
            if dir_path.exists() {
                tokio::fs::remove_dir_all(&dir_path).await
                    .context("清理无效版本失败")?;
            }
        }

        // 按版本排序
        installed_versions.sort_by(|a, b| b.0.cmp(&a.0));

        let current_version = installed_versions.first().map(|(v, _)| v.clone());
        let install_path = current_version.as_ref()
            .map(|_| self.install_dir.join(installed_versions[0].1.as_str()));

        // 检查是否满足最小版本要求
        let meets_minimum = if let (Some(current), Some(minimum)) = (&current_version, &self.config.minimum_version) {
            current >= minimum
        } else {
            true
        };

        let needs_update = if let Some(_current) = &current_version {
            // 这里可以检查最新版本
            false // 简化实现
        } else {
            true
        };

        let status_message = if installed_versions.is_empty() {
            "未安装".to_string()
        } else if !meets_minimum {
            "版本过低，需要更新".to_string()
        } else if needs_update {
            "有新版本可用".to_string()
        } else {
            "已安装最新版本".to_string()
        };

        Ok(ClaudeCodeAcpStatus {
            is_installed: !installed_versions.is_empty() && meets_minimum,
            current_version,
            latest_version: None, // 可以通过 npm API 获取
            install_path,
            needs_update,
            status_message,
        })
    }

    /// 安装或更新 Claude Code ACP
    pub async fn install_or_update(&self) -> Result<ClaudeCodeAcpStatus> {
        let status = self.get_status().await?;

        if status.is_installed && !status.needs_update {
            info!("Claude Code ACP 已是最新版本");
            return Ok(status);
        }

        info!("开始安装 Claude Code ACP");

        // 下载最新版本
        let version = self.download_latest_version().await
            .context("下载最新版本失败")?;

        // 验证安装
        let agent_server_path = self.install_dir.join(&version).join(&self.config.entrypoint_path);
        if !agent_server_path.exists() {
            return Err(anyhow!("安装失败：入口文件不存在: {:?}", agent_server_path));
        }

        info!("Claude Code ACP 安装完成，版本: {}", version);
        self.get_status().await
    }

    /// 获取命令
    pub async fn get_command(&self) -> Result<ClaudeCodeAcpCommand> {
        let status = self.get_status().await?;

        if !status.is_installed {
            return Err(anyhow!("Claude Code ACP 未安装，请先调用 install_or_update"));
        }

        let install_path = status.install_path
            .ok_or_else(|| anyhow!("无法确定安装路径"))?;

        let node_path = self.find_node_executable().await
            .context("未找到 Node.js 可执行文件")?;

        Ok(ClaudeCodeAcpCommand {
            path: node_path,
            args: vec![install_path.join(&self.config.entrypoint_path)
                .to_string_lossy()
                .to_string()],
            env: None,
        })
    }

    /// 检查是否可用
    pub async fn is_available(&self) -> bool {
        match self.get_status().await {
            Ok(status) => status.is_installed,
            Err(_) => false,
        }
    }

    /// 下载最新版本
    async fn download_latest_version(&self) -> Result<String> {
        debug!("下载最新版本的 {}", self.config.package_name);

        // 创建临时目录
        let temp_dir = tempfile::tempdir_in(&self.install_dir)
            .context("创建临时目录失败")?;

        // 使用 npm 安装
        let output = tokio::process::Command::new("npm")
            .args(["install", &self.config.package_name])
            .current_dir(temp_dir.path())
            .output()
            .await
            .context("npm install 失败")?;

        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("npm install 失败: {}", error_msg));
        }

        // 获取安装的版本
        let package_json_path = temp_dir.path()
            .join("node_modules")
            .join(&self.config.package_name)
            .join("package.json");

        let package_json_content = tokio::fs::read_to_string(&package_json_path).await
            .context("读取 package.json 失败")?;

        let package_json: serde_json::Value = serde_json::from_str(&package_json_content)
            .context("解析 package.json 失败")?;

        let version = package_json.get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("package.json 中缺少 version 字段"))?;

        // 移动到最终位置
        let target_dir = self.install_dir.join(version);
        tokio::fs::create_dir_all(&target_dir).await
            .context("创建目标目录失败")?;

        let node_modules_dir = temp_dir.path().join("node_modules");
        Box::pin(self.copy_dir_recursive(&node_modules_dir, &target_dir)).await
            .context("复制文件失败")?;

        Ok(version.to_string())
    }

    /// 查找 Node.js 可执行文件
    async fn find_node_executable(&self) -> Result<PathBuf> {
        // 首先检查 PATH 中的 node
        if let Ok(path) = which::which("node") {
            return Ok(path);
        }

        // 检查常见的 Node.js 安装路径
        let common_paths = [
            "/usr/local/bin/node",
            "/opt/homebrew/bin/node",
            "/usr/bin/node",
            "C:\\Program Files\\nodejs\\node.exe",
        ];

        for path in common_paths.iter() {
            if Path::new(path).exists() {
                return Ok(PathBuf::from(path));
            }
        }

        Err(anyhow!("未找到 Node.js 可执行文件"))
    }

    /// 递归复制目录
    async fn copy_dir_recursive(&self, src: &Path, dst: &Path) -> Result<()> {
        let mut entries = tokio::fs::read_dir(src).await?;

        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());

            if file_type.is_dir() {
                tokio::fs::create_dir_all(&dst_path).await?;
                Box::pin(self.copy_dir_recursive(&src_path, &dst_path)).await?;
            } else {
                tokio::fs::copy(&src_path, &dst_path).await?;
            }
        }

        Ok(())
    }

    /// 清理旧版本
    pub async fn cleanup_old_versions(&self) -> Result<()> {
        let status = self.get_status().await?;

        if let Some(current_version) = &status.current_version {
            let mut entries = tokio::fs::read_dir(&self.install_dir).await?;

            while let Some(entry) = entries.next_entry().await? {
                let file_name = entry.file_name();
                let Some(file_name_str) = file_name.to_str() else { continue };

                if let Ok(version) = Version::from_str(file_name_str) {
                    if version != *current_version {
                        let dir_path = self.install_dir.join(file_name_str);
                        if dir_path.exists() {
                            tokio::fs::remove_dir_all(&dir_path).await
                                .context("清理旧版本失败")?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// 获取登录命令
    pub fn get_login_command(&self, command: &ClaudeCodeAcpCommand) -> Option<ClaudeCodeAcpCommand> {
        // 从命令参数中提取路径前缀
        let path_prefix = command.args.first()
            .and_then(|path| path.strip_suffix("/@zed-industries/claude-code-acp/dist/index.js"))?;

        Some(ClaudeCodeAcpCommand {
            path: command.path.clone(),
            args: vec![
                Path::new(path_prefix)
                    .join("@anthropic-ai/claude-code/cli.js")
                    .to_string_lossy()
                    .to_string(),
                "/login".to_string(),
            ],
            env: command.env.clone(),
        })
    }
}

/// 全局 Claude Code ACP 管理器实例
static GLOBAL_CLAUDE_ACP_MANAGER: OnceLock<ClaudeCodeAcpManager> = OnceLock::new();

/// 获取全局 Claude Code ACP 管理器
pub fn get_global_claude_acp_manager() -> &'static ClaudeCodeAcpManager {
    GLOBAL_CLAUDE_ACP_MANAGER.get_or_init(|| ClaudeCodeAcpManager::default())
}

/// 便捷函数：确保 Claude Code ACP 已安装
pub async fn ensure_claude_acp_installed() -> Result<ClaudeCodeAcpCommand> {
    let manager = get_global_claude_acp_manager();

    if !manager.is_available().await {
        info!("Claude Code ACP 未安装，正在安装...");
        manager.install_or_update().await?;
    }

    manager.get_command().await
}

/// 便捷函数：获取 Claude Code ACP 命令
pub async fn get_claude_acp_command() -> Result<ClaudeCodeAcpCommand> {
    let manager = get_global_claude_acp_manager();
    manager.get_command().await
}

/// 便捷函数：检查 Claude Code ACP 状态
pub async fn check_claude_acp_status() -> Result<ClaudeCodeAcpStatus> {
    let manager = get_global_claude_acp_manager();
    manager.get_status().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::fs;

    #[tokio::test]
    async fn test_claude_acp_manager_creation() {
        let manager = ClaudeCodeAcpManager::default();
        assert_eq!(manager.config.package_name, "@zed-industries/claude-code-acp");
        assert_eq!(manager.config.binary_name, "claude-code-acp");
    }

    #[tokio::test]
    async fn test_get_status() {
        let manager = ClaudeCodeAcpManager::default();
        let status = manager.get_status().await.unwrap();
        // 状态检查应该不会崩溃
        println!("Status: {:?}", status);
    }

    #[test]
    fn test_global_manager() {
        let manager1 = get_global_claude_acp_manager();
        let manager2 = get_global_claude_acp_manager();

        // 应该是同一个实例
        assert_eq!(manager1 as *const ClaudeCodeAcpManager,
                   manager2 as *const ClaudeCodeAcpManager);
    }

    #[tokio::test]
    async fn test_custom_config() {
        let config = ClaudeCodeAcpConfig {
            minimum_version: Some("1.0.0".parse().unwrap()),
            package_name: "test-package".to_string(),
            entrypoint_path: PathBuf::from("dist/index.js"),
            binary_name: "test-binary".to_string(),
            custom_command: Some("custom-command".to_string()),
            ignore_system_version: true,
        };

        let manager = ClaudeCodeAcpManager::new(config);
        assert_eq!(manager.config.package_name, "test-package");
        assert_eq!(manager.config.binary_name, "test-binary");
        assert_eq!(manager.config.custom_command, Some("custom-command".to_string()));
    }

    #[tokio::test]
    async fn test_status_when_not_installed() {
        let temp_dir = TempDir::new().unwrap();
        let install_dir = temp_dir.path().join("test-agent");

        let config = ClaudeCodeAcpConfig {
            package_name: "@test/nonexistent-package".to_string(),
            entrypoint_path: PathBuf::from("dist/index.js"),
            binary_name: "test-agent".to_string(),
            ..Default::default()
        };

        let manager = ClaudeCodeAcpManager {
            config,
            install_dir,
        };

        let status = manager.get_status().await.unwrap();
        assert!(!status.is_installed);
        assert_eq!(status.status_message, "未安装");
    }

    #[tokio::test]
    async fn test_status_with_mock_installation() {
        let temp_dir = TempDir::new().unwrap();
        let install_dir = temp_dir.path().join("test-agent");
        let version_dir = install_dir.join("1.0.0");
        let entrypoint_path = version_dir.join("node_modules/test-package/dist/index.js");

        // 创建模拟安装结构
        fs::create_dir_all(entrypoint_path.parent().unwrap()).unwrap();
        fs::write(&entrypoint_path, "mock entrypoint").unwrap();

        let config = ClaudeCodeAcpConfig {
            package_name: "test-package".to_string(),
            entrypoint_path: PathBuf::from("node_modules/test-package/dist/index.js"),
            binary_name: "test-agent".to_string(),
            minimum_version: Some("0.1.0".parse().unwrap()),
            ..Default::default()
        };

        let manager = ClaudeCodeAcpManager {
            config,
            install_dir,
        };

        let status = manager.get_status().await.unwrap();
        assert!(status.is_installed);
        assert_eq!(status.current_version, Some("1.0.0".parse().unwrap()));
        assert_eq!(status.status_message, "已安装最新版本");
    }

    #[tokio::test]
    async fn test_status_with_version_below_minimum() {
        let temp_dir = TempDir::new().unwrap();
        let install_dir = temp_dir.path().join("test-agent");
        let version_dir = install_dir.join("0.1.0");
        let entrypoint_path = version_dir.join("node_modules/test-package/dist/index.js");

        // 创建模拟安装结构，版本低于最小要求
        fs::create_dir_all(entrypoint_path.parent().unwrap()).unwrap();
        fs::write(&entrypoint_path, "mock entrypoint").unwrap();

        let config = ClaudeCodeAcpConfig {
            package_name: "test-package".to_string(),
            entrypoint_path: PathBuf::from("node_modules/test-package/dist/index.js"),
            binary_name: "test-agent".to_string(),
            minimum_version: Some("1.0.0".parse().unwrap()), // 最小版本高于安装版本
            ..Default::default()
        };

        let manager = ClaudeCodeAcpManager {
            config,
            install_dir,
        };

        let status = manager.get_status().await.unwrap();
        assert!(!status.is_installed); // 不满足最小版本要求
        assert_eq!(status.current_version, Some("0.1.0".parse().unwrap()));
        assert_eq!(status.status_message, "版本过低，需要更新");
    }

    #[tokio::test]
    async fn test_cleanup_invalid_installations() {
        let temp_dir = TempDir::new().unwrap();
        let install_dir = temp_dir.path().join("test-agent");

        // 创建有效的安装
        let valid_version_dir = install_dir.join("1.0.0");
        let valid_entrypoint = valid_version_dir.join("node_modules/test-package/dist/index.js");
        fs::create_dir_all(valid_entrypoint.parent().unwrap()).unwrap();
        fs::write(&valid_entrypoint, "valid entrypoint").unwrap();

        // 创建无效的安装（缺少入口文件）
        let invalid_version_dir = install_dir.join("invalid-version");
        let invalid_entrypoint = invalid_version_dir.join("node_modules/test-package/dist/index.js");
        fs::create_dir_all(invalid_entrypoint.parent().unwrap()).unwrap();
        // 故意不创建入口文件

        // 创建非版本目录
        let non_version_dir = install_dir.join("not-a-version");
        fs::create_dir_all(&non_version_dir).unwrap();

        let config = ClaudeCodeAcpConfig {
            package_name: "test-package".to_string(),
            entrypoint_path: PathBuf::from("node_modules/test-package/dist/index.js"),
            binary_name: "test-agent".to_string(),
            ..Default::default()
        };

        let manager = ClaudeCodeAcpManager {
            config,
            install_dir: install_dir.clone(),
        };

        // 获取状态会自动清理无效安装
        let status = manager.get_status().await.unwrap();

        // 验证有效安装仍然存在
        assert!(valid_entrypoint.exists());

        // 验证无效安装已被清理
        assert!(!invalid_version_dir.exists());
        assert!(!non_version_dir.exists());

        // 验证状态正确
        assert!(status.is_installed);
        assert_eq!(status.current_version, Some("1.0.0".parse().unwrap()));
    }

    #[tokio::test]
    async fn test_login_command_generation() {
        let command = ClaudeCodeAcpCommand {
            path: PathBuf::from("/usr/local/bin/node"),
            args: vec![
                "/tmp/claude-code-acp/1.0.0/node_modules/@zed-industries/claude-code-acp/dist/index.js".to_string()
            ],
            env: Some(std::collections::HashMap::new()),
        };

        let config = ClaudeCodeAcpConfig::default();
        let manager = ClaudeCodeAcpManager::new(config);

        let login_command = manager.get_login_command(&command);

        assert!(login_command.is_some());
        let login_cmd = login_command.unwrap();

        assert_eq!(login_cmd.path, PathBuf::from("/usr/local/bin/node"));
        assert_eq!(login_cmd.args.len(), 2);
        assert!(login_cmd.args[0].contains("@anthropic-ai/claude-code/cli.js"));
        assert_eq!(login_cmd.args[1], "/login");
    }

    #[tokio::test]
    async fn test_login_command_generation_with_invalid_path() {
        let command = ClaudeCodeAcpCommand {
            path: PathBuf::from("/usr/local/bin/node"),
            args: vec![
                "/tmp/claude-code-acp/1.0.0/wrong/path/index.js".to_string()
            ],
            env: None,
        };

        let config = ClaudeCodeAcpConfig::default();
        let manager = ClaudeCodeAcpManager::new(config);

        let login_command = manager.get_login_command(&command);

        // 路径不匹配预期模式，应该返回 None
        assert!(login_command.is_none());
    }

    // 这个测试模拟了安装流程，但不会真正执行 npm install
    #[tokio::test]
    async fn test_install_or_update_simulation() {
        let temp_dir = TempDir::new().unwrap();
        let install_dir = temp_dir.path().join("test-agent");

        let config = ClaudeCodeAcpConfig {
            package_name: "test-package".to_string(),
            entrypoint_path: PathBuf::from("node_modules/test-package/dist/index.js"),
            binary_name: "test-agent".to_string(),
            minimum_version: Some("1.0.0".parse().unwrap()),
            ..Default::default()
        };

        let manager = ClaudeCodeAcpManager {
            config,
            install_dir,
        };

        // 初始状态应该显示未安装
        let initial_status = manager.get_status().await.unwrap();
        assert!(!initial_status.is_installed);

        // 注意：这里我们不会真正测试安装过程，因为那需要网络连接和 npm
        // 在实际项目中，你可以使用 mock 来模拟 npm install 的行为
        // 这里只是验证结构正确性
        assert_eq!(initial_status.status_message, "未安装");
    }

    #[tokio::test]
    async fn test_node_executable_finding() {
        let config = ClaudeCodeAcpConfig::default();
        let manager = ClaudeCodeAcpManager::new(config);

        // 这个测试依赖于系统上是否安装了 Node.js
        // 在 CI 环境中可能会失败，但我们只是测试函数不会崩溃
        let result = manager.find_node_executable().await;

        // 无论是否找到 Node.js，函数都应该正常返回
        match result {
            Ok(path) => {
                println!("Found Node.js at: {:?}", path);
                assert!(path.exists());
            }
            Err(_) => {
                println!("Node.js not found, which is acceptable in test environment");
            }
        }
    }
}