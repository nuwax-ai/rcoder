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
        info!("Checking Agent installation status: {}", command);

        // First, check if the command already exists
        if self.is_command_available(command).await {
            // Validate if it's working correctly
            if self.validate_installation(config, command).await? {
                info!("Agent is installed and verified: {}", command);
                return Ok(InstallResult::already_installed(None));
            }
            warn!(
                "Agent command exists but verification failed, trying reinstall: {}",
                command
            );
        }

        // Not installed or validation failed, try to install
        info!("Trying to install Agent: {}", command);
        let result = self.install(config, None).await;

        match &result {
            Ok(install_result) => {
                if install_result.success {
                    info!(
                        "Agent installed successfully: {} - {}",
                        command, install_result.message
                    );
                } else {
                    warn!(
                        "⚠️ Agent Installation failed: {} - {}",
                        command, install_result.message
                    );
                }
            }
            Err(e) => {
                warn!("Agent installation error: {} - {}", command, e);
            }
        }

        result
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
            "Starting Agent installation: {} via {}",
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
        debug!("Verifying Agent installation: {}", command);

        // If validate_command is specified, use it
        if let Some(validate_cmd) = &config.validate_command
            && !validate_cmd.is_empty() {
                return self.run_validation_command(validate_cmd).await;
            }

        // 使用 which 检查命令是否可用（适用于 Debian/Docker 环境）
        // which::which 是 Rust crate，不依赖系统 which 命令
        // 注意：只检查命令是否在 PATH 中，不运行命令避免副作用（如 claude-code-acp-ts --version 会阻塞）
        if self.is_command_available(command).await {
            debug!("Command exists in PATH: {}", command);
            return Ok(true);
        }

        debug!("Command not in PATH: {}", command);
        Ok(false)
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

        debug!("Running verification command: {} {:?}", program, args);

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

        info!("Updating Agent via {}", installer.package_manager_name());

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
