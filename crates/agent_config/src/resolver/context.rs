//! Resolution context for variable substitution.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Project context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContext {
    /// Project ID
    pub project_id: String,

    /// Project name
    pub project_name: String,

    /// Project path
    pub project_path: PathBuf,
}

impl ProjectContext {
    /// Create a new project context
    pub fn new<P: Into<PathBuf>>(
        project_id: String,
        project_name: String,
        project_path: P,
    ) -> Self {
        Self {
            project_id,
            project_name,
            project_path: project_path.into(),
        }
    }

    /// Get project directory name
    pub fn project_dir_name(&self) -> String {
        self.project_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.project_id)
            .to_string()
    }
}

/// Resolution context for variable substitution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolutionContext {
    /// Model provider configuration
    pub model_provider: shared_types::ModelProviderConfig,

    /// Project context
    pub project_context: ProjectContext,

    /// Custom variables
    pub custom_variables: HashMap<String, String>,

    /// MCP server variables
    pub mcp_variables: HashMap<String, String>,
}

impl ResolutionContext {
    /// Create a new resolution context
    pub fn new(
        model_provider: shared_types::ModelProviderConfig,
        project_context: ProjectContext,
    ) -> Self {
        Self {
            model_provider,
            project_context,
            custom_variables: HashMap::new(),
            mcp_variables: HashMap::new(),
        }
    }

    /// Add a custom variable
    pub fn with_custom_variable(mut self, key: String, value: String) -> Self {
        self.custom_variables.insert(key, value);
        self
    }

    /// Add an MCP variable
    pub fn with_mcp_variable(mut self, key: String, value: String) -> Self {
        self.mcp_variables.insert(key, value);
        self
    }
}
