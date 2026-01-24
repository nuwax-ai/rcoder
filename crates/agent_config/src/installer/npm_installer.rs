//! NPM Package Installer
//!
//! Implements agent installation via npm.

use std::path::Path;

use tracing::{debug, info, warn};

use super::manager::{InstallResult, InstallationError};
use super::traits::AgentInstaller;
use crate::types::installation::InstallationConfig;

/// NPM Installer
///
/// Installs agents via npm global installation.
pub struct NpmInstaller {
    /// Use pnpm instead of npm if available
    prefer_pnpm: bool,
}

impl NpmInstaller {
    /// Create a new NPM installer
    pub fn new() -> Self {
        Self { prefer_pnpm: false }
    }

    /// Create NPM installer that prefers pnpm
    pub fn with_pnpm_preference() -> Self {
        Self { prefer_pnpm: true }
    }

    /// Get the npm command to use (npm or pnpm)
    async fn get_npm_command(&self) -> &'static str {
        if self.prefer_pnpm && which::which("pnpm").is_ok() {
            "pnpm"
        } else if which::which("npm").is_ok() {
            "npm"
        } else {
            "npm" // Default, will fail if not available
        }
    }

    /// Parse version from npm output
    fn parse_installed_version(&self, output: &str) -> Option<String> {
        // npm list output format: "package@version"
        for line in output.lines() {
            if line.contains('@') {
                if let Some(version) = line.split('@').last() {
                    return Some(version.trim().to_string());
                }
            }
        }
        None
    }
}

impl Default for NpmInstaller {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AgentInstaller for NpmInstaller {
    async fn install(
        &self,
        config: &InstallationConfig,
        _install_dir: Option<&Path>,
    ) -> Result<InstallResult, InstallationError> {
        let npm_cmd = self.get_npm_command().await;

        // Check if npm is available
        if !self.command_exists(npm_cmd).await {
            return Err(InstallationError::PackageManagerNotAvailable(
                npm_cmd.to_string(),
            ));
        }

        let package_name = config
            .package_name
            .as_ref()
            .ok_or_else(|| InstallationError::ConfigError("Package name is required".to_string()))?;

        // Build package spec with version
        let package_spec = if let Some(version) = &config.version {
            if version == "latest" {
                package_name.clone()
            } else {
                format!("{}@{}", package_name, version)
            }
        } else {
            package_name.clone()
        };

        info!("📦 使用 {} 全局安装: {}", npm_cmd, package_spec);

        // Run npm install -g
        // 仅 nuwaxcode 使用官方源（--registry 参数优先级高于 .npmrc）
        let output = if package_name == "nuwaxcode" {
            info!("🌐 nuwaxcode 使用官方源: https://registry.npmjs.org/");
            self.run_command(
                npm_cmd,
                &["install", "-g", &package_spec, "--registry=https://registry.npmjs.org/"],
            )
            .await?
        } else {
            self.run_command(npm_cmd, &["install", "-g", &package_spec]).await?
        };

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            debug!("npm install stdout: {}", stdout);
            if !stderr.is_empty() {
                debug!("npm install stderr: {}", stderr);
            }

            // Try to get installed version
            let version = self.parse_installed_version(&stdout);

            info!("✅ 安装成功: {}", package_spec);
            Ok(InstallResult::success(
                version,
                format!("Successfully installed {}", package_spec),
            ))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("❌ 安装失败: {}", stderr);
            Err(InstallationError::InstallFailed {
                package: package_spec,
                reason: stderr.to_string(),
            })
        }
    }

    async fn validate(&self, config: &InstallationConfig) -> Result<bool, InstallationError> {
        // Check if the package is globally installed
        let npm_cmd = self.get_npm_command().await;

        if !self.command_exists(npm_cmd).await {
            return Ok(false);
        }

        let package_name = match &config.package_name {
            Some(name) => name,
            None => return Ok(false),
        };

        // Check global list
        let output = self.run_command(npm_cmd, &["list", "-g", "--depth=0", package_name]).await?;

        Ok(output.status.success())
    }

    async fn update(
        &self,
        config: &InstallationConfig,
        _install_dir: Option<&Path>,
    ) -> Result<InstallResult, InstallationError> {
        let npm_cmd = self.get_npm_command().await;

        if !self.command_exists(npm_cmd).await {
            return Err(InstallationError::PackageManagerNotAvailable(
                npm_cmd.to_string(),
            ));
        }

        let package_name = config
            .package_name
            .as_ref()
            .ok_or_else(|| InstallationError::ConfigError("Package name is required".to_string()))?;

        info!("🔄 更新全局包: {}", package_name);

        // Run npm update -g
        // 仅 nuwaxcode 使用官方源（--registry 参数优先级高于 .npmrc）
        let output = if package_name == "nuwaxcode" {
            info!("🌐 nuwaxcode 使用官方源: https://registry.npmjs.org/");
            self.run_command(
                npm_cmd,
                &["update", "-g", package_name, "--registry=https://registry.npmjs.org/"],
            )
            .await?
        } else {
            self.run_command(npm_cmd, &["update", "-g", package_name]).await?
        };

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let version = self.parse_installed_version(&stdout);

            info!("✅ 更新成功: {}", package_name);
            Ok(InstallResult::success(
                version,
                format!("Successfully updated {}", package_name),
            ))
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("❌ 更新失败: {}", stderr);
            Err(InstallationError::InstallFailed {
                package: package_name.clone(),
                reason: stderr.to_string(),
            })
        }
    }

    fn package_manager_name(&self) -> &'static str {
        "npm"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_npm_command_detection() {
        let installer = NpmInstaller::new();
        let cmd = installer.get_npm_command().await;
        // Should return "npm" or "pnpm"
        assert!(cmd == "npm" || cmd == "pnpm");
    }

    #[test]
    fn test_version_parsing() {
        let installer = NpmInstaller::new();

        let output = "└── @zed-industries/claude-code-acp@0.6.0";
        let version = installer.parse_installed_version(output);
        assert_eq!(version, Some("0.6.0".to_string()));
    }
}
