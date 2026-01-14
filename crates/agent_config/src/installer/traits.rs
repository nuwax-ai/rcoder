//! Agent Installer Trait
//!
//! Defines the interface for agent installers.

use std::path::Path;

use super::manager::{InstallResult, InstallationError};
use crate::types::installation::InstallationConfig;

/// Agent installer trait
///
/// Defines the interface for installing, validating, and updating agents.
#[async_trait::async_trait]
pub trait AgentInstaller: Send + Sync {
    /// Install the agent
    ///
    /// # Arguments
    /// * `config` - Installation configuration
    /// * `install_dir` - Directory to install to (optional, for global installs)
    ///
    /// # Returns
    /// Installation result with details
    async fn install(
        &self,
        config: &InstallationConfig,
        install_dir: Option<&Path>,
    ) -> Result<InstallResult, InstallationError>;

    /// Validate if the agent is installed correctly
    ///
    /// # Arguments
    /// * `config` - Installation configuration containing validate_command
    ///
    /// # Returns
    /// true if installed and working, false otherwise
    async fn validate(&self, config: &InstallationConfig) -> Result<bool, InstallationError>;

    /// Update the agent to latest version
    ///
    /// # Arguments
    /// * `config` - Installation configuration
    /// * `install_dir` - Directory where agent is installed
    ///
    /// # Returns
    /// Installation result with new version info
    async fn update(
        &self,
        config: &InstallationConfig,
        install_dir: Option<&Path>,
    ) -> Result<InstallResult, InstallationError>;

    /// Get the package manager name
    fn package_manager_name(&self) -> &'static str;

    /// Check if a command exists in PATH
    async fn command_exists(&self, command: &str) -> bool {
        which::which(command).is_ok()
    }

    /// Run a command and get output
    async fn run_command(
        &self,
        command: &str,
        args: &[&str],
    ) -> Result<std::process::Output, InstallationError> {
        tokio::process::Command::new(command)
            .args(args)
            .output()
            .await
            .map_err(|e| InstallationError::CommandFailed {
                command: format!("{} {}", command, args.join(" ")),
                reason: e.to_string(),
            })
    }
}
