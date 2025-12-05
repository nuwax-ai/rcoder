//! Agent Installation Manager
//!
//! Manages agent installation, validation, and updates.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use thiserror::Error;
use tracing::{debug, info, warn};

use super::npm_installer::NpmInstaller;
use super::traits::AgentInstaller;
use crate::types::installation::{InstallationConfig, PackageManager};

/// Installation error types
#[derive(Error, Debug)]
pub enum InstallationError {
    #[error("Package manager '{0}' is not available")]
    PackageManagerNotAvailable(String),

    #[error("Package '{package}' not found in {manager}")]
    PackageNotFound { package: String, manager: String },

    #[error("Installation failed for '{package}': {reason}")]
    InstallFailed { package: String, reason: String },

    #[error("Validation failed: {0}")]
    ValidationFailed(String),

    #[error("Command failed: {command} - {reason}")]
    CommandFailed { command: String, reason: String },

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Unsupported package manager: {0}")]
    UnsupportedPackageManager(String),
}

/// Installation result
#[derive(Debug, Clone)]
pub struct InstallResult {
    /// Whether installation was successful
    pub success: bool,
    /// Installed version
    pub version: Option<String>,
    /// Installation path
    pub install_path: Option<String>,
    /// Additional message
    pub message: String,
    /// Whether the agent was already installed
    pub already_installed: bool,
}

impl InstallResult {
    /// Create a successful installation result
    pub fn success(version: Option<String>, message: impl Into<String>) -> Self {
        Self {
            success: true,
            version,
            install_path: None,
            message: message.into(),
            already_installed: false,
        }
    }

    /// Create a result indicating already installed
    pub fn already_installed(version: Option<String>) -> Self {
        Self {
            success: true,
            version,
            install_path: None,
            message: "Already installed".to_string(),
            already_installed: true,
        }
    }

    /// Create a failed installation result
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            success: false,
            version: None,
            install_path: None,
            message: message.into(),
            already_installed: false,
        }
    }

    /// Set installation path
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.install_path = Some(path.into());
        self
    }
}

/// Agent Installation Manager
///
/// Manages multiple package manager installers and handles
/// agent installation lifecycle.
pub struct AgentInstallationManager {
    installers: HashMap<String, Arc<dyn AgentInstaller>>,
}

impl AgentInstallationManager {
    /// Create a new installation manager with default installers
    pub fn new() -> Self {
        let mut installers: HashMap<String, Arc<dyn AgentInstaller>> = HashMap::new();

        // Register npm installer
        installers.insert("npm".to_string(), Arc::new(NpmInstaller::new()));

        Self { installers }
    }

    /// Register a custom installer
    pub fn register_installer(
        &mut self,
        name: impl Into<String>,
        installer: Arc<dyn AgentInstaller>,
    ) {
        self.installers.insert(name.into(), installer);
    }

    /// Get installer for package manager
    fn get_installer(&self, pm: &PackageManager) -> Option<Arc<dyn AgentInstaller>> {
        let name = pm.as_str();
        self.installers.get(name).cloned()
    }

    /// Ensure agent is installed
    ///
    /// This method checks if the agent is already installed and working.
    /// If not, it attempts to install it.
    pub async fn ensure_installed(
        &self,
        config: &InstallationConfig,
        command: &str,
    ) -> Result<InstallResult, InstallationError> {
        info!("🔍 检查 Agent 安装状态: {}", command);

        // First, check if the command already exists
        if self.is_command_available(command).await {
            // Validate if it's working correctly
            if self.validate_installation(config, command).await? {
                info!("✅ Agent 已安装且验证通过: {}", command);
                return Ok(InstallResult::already_installed(None));
            }
            warn!("⚠️ Agent 命令存在但验证失败，尝试重新安装: {}", command);
        }

        // Not installed or validation failed, try to install
        self.install(config, None).await
    }

    /// Install agent using appropriate package manager
    pub async fn install(
        &self,
        config: &InstallationConfig,
        install_dir: Option<&Path>,
    ) -> Result<InstallResult, InstallationError> {
        let installer = self.get_installer(&config.package_manager).ok_or_else(|| {
            InstallationError::UnsupportedPackageManager(
                config.package_manager.as_str().to_string(),
            )
        })?;

        let package_name = config.package_name.as_ref().ok_or_else(|| {
            InstallationError::ConfigError("Package name is required".to_string())
        })?;

        info!(
            "📦 开始安装 Agent: {} via {}",
            package_name,
            installer.package_manager_name()
        );

        installer.install(config, install_dir).await
    }

    /// Validate agent installation
    pub async fn validate_installation(
        &self,
        config: &InstallationConfig,
        command: &str,
    ) -> Result<bool, InstallationError> {
        debug!("🔍 验证 Agent 安装: {}", command);

        // If validate_command is specified, use it
        if let Some(validate_cmd) = &config.validate_command {
            if !validate_cmd.is_empty() {
                return self.run_validation_command(validate_cmd).await;
            }
        }

        // 使用 which 检查命令是否可用（适用于 Debian/Docker 环境）
        // which::which 是 Rust crate，不依赖系统 which 命令
        if self.is_command_available(command).await {
            debug!("✅ 命令存在于 PATH 中: {}", command);
            return Ok(true);
        }

        // 如果 which 失败，尝试运行 --version 作为后备验证
        let result = self.try_version_check(command).await;
        Ok(result)
    }

    /// Check if a command is available in PATH
    async fn is_command_available(&self, command: &str) -> bool {
        which::which(command).is_ok()
    }

    /// Run custom validation command
    async fn run_validation_command(&self, cmd: &[String]) -> Result<bool, InstallationError> {
        if cmd.is_empty() {
            return Ok(false);
        }

        let program = &cmd[0];
        let args: Vec<&str> = cmd[1..].iter().map(|s| s.as_str()).collect();

        debug!("运行验证命令: {} {:?}", program, args);

        let output = tokio::process::Command::new(program)
            .args(&args)
            .output()
            .await
            .map_err(|e| InstallationError::CommandFailed {
                command: format!("{} {}", program, args.join(" ")),
                reason: e.to_string(),
            })?;

        Ok(output.status.success())
    }

    /// Try running command with --version to check if it works
    async fn try_version_check(&self, command: &str) -> bool {
        // Try --version first
        if let Ok(output) = tokio::process::Command::new(command)
            .arg("--version")
            .output()
            .await
        {
            if output.status.success() {
                let version = String::from_utf8_lossy(&output.stdout);
                debug!("版本检查成功: {} -> {}", command, version.trim());
                return true;
            }
        }

        // Try --help as fallback
        if let Ok(output) = tokio::process::Command::new(command)
            .arg("--help")
            .output()
            .await
        {
            return output.status.success();
        }

        false
    }

    /// Update agent to latest version
    pub async fn update(
        &self,
        config: &InstallationConfig,
        install_dir: Option<&Path>,
    ) -> Result<InstallResult, InstallationError> {
        let installer = self.get_installer(&config.package_manager).ok_or_else(|| {
            InstallationError::UnsupportedPackageManager(
                config.package_manager.as_str().to_string(),
            )
        })?;

        info!("🔄 更新 Agent via {}", installer.package_manager_name());

        installer.update(config, install_dir).await
    }
}

impl Default for AgentInstallationManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_command_availability() {
        let manager = AgentInstallationManager::new();
        // Test with a command that should exist on most systems
        let ls_available = manager.is_command_available("ls").await;
        assert!(ls_available);

        // Test with a command that shouldn't exist
        let fake_available = manager
            .is_command_available("this_command_does_not_exist_12345")
            .await;
        assert!(!fake_available);
    }
}
