//! Installation configuration structures.

use serde::{Deserialize, Serialize};

/// Package manager type
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    /// npm package manager
    Npm,
    /// Local binary
    Local,
    /// Custom package manager
    Custom(String),
}

/// Installation configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationConfig {
    /// Package manager
    pub package_manager: PackageManager,

    /// Package name
    #[serde(default)]
    pub package_name: Option<String>,

    /// Version constraint
    #[serde(default)]
    pub version: Option<String>,

    /// Installation source (for custom sources)
    #[serde(default)]
    pub source: Option<String>,

    /// Validation command
    #[serde(default)]
    pub validate_command: Option<Vec<String>>,

    /// Whether to auto-update
    #[serde(default)]
    pub auto_update: bool,
}

impl Default for InstallationConfig {
    fn default() -> Self {
        Self {
            package_manager: PackageManager::Npm,
            package_name: None,
            version: None,
            source: None,
            validate_command: None,
            auto_update: false,
        }
    }
}

impl InstallationConfig {
    /// Create npm installation config
    pub fn npm(package_name: String) -> Self {
        Self {
            package_manager: PackageManager::Npm,
            package_name: Some(package_name),
            version: Some("latest".to_string()),
            source: None,
            validate_command: None,
            auto_update: false,
        }
    }

    /// Create local installation config
    pub fn local(command: String) -> Self {
        Self {
            package_manager: PackageManager::Local,
            package_name: Some(command),
            version: None,
            source: None,
            validate_command: None,
            auto_update: false,
        }
    }

    /// Create custom installation config
    pub fn custom(package_manager: String, source: String) -> Self {
        Self {
            package_manager: PackageManager::Custom(package_manager),
            package_name: None,
            version: None,
            source: Some(source),
            validate_command: None,
            auto_update: false,
        }
    }

    /// Set version
    pub fn with_version(mut self, version: String) -> Self {
        self.version = Some(version);
        self
    }

    /// Set validation command
    pub fn with_validation(mut self, command: Vec<String>) -> Self {
        self.validate_command = Some(command);
        self
    }

    /// Enable auto-update
    pub fn with_auto_update(mut self) -> Self {
        self.auto_update = true;
        self
    }
}

impl PackageManager {
    /// Get the manager name as string
    pub fn as_str(&self) -> &str {
        match self {
            PackageManager::Npm => "npm",
            PackageManager::Local => "local",
            PackageManager::Custom(name) => name,
        }
    }
}
