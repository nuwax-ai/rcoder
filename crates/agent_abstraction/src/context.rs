//! Agent context information.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Agent context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentContext {
    /// Project ID
    pub project_id: String,

    /// Project path
    pub project_path: PathBuf,

    /// Timestamp when context was created
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl AgentContext {
    /// Create a new agent context
    pub fn new<P: Into<PathBuf>>(project_id: String, project_path: P) -> Self {
        Self {
            project_id,
            project_path: project_path.into(),
            timestamp: chrono::Utc::now(),
        }
    }

    /// Get the project directory name
    pub fn project_dir_name(&self) -> String {
        self.project_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&self.project_id)
            .to_string()
    }
}
