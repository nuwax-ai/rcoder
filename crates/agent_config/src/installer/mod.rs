//! Agent Installation Management
//!
//! This module provides automatic installation and validation for agents.

mod manager;
mod npm_installer;
mod traits;

pub use manager::{AgentInstallationManager, InstallResult, InstallationError};
pub use npm_installer::NpmInstaller;
pub use traits::AgentInstaller;
