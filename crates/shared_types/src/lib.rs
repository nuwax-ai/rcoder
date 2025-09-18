use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;
use chrono;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
    pub description: Option<String>,
    pub template: Option<String>,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRequest {
    pub project_id: Option<Uuid>,
    pub prompt: String,
    pub context: Option<PromptContext>,
    pub auto_create: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptContext {
    pub files: Vec<PathBuf>,
    pub current_file: Option<PathBuf>,
    pub selected_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub status: PromptStatus,
    pub message: Option<String>,
    pub changes: Vec<FileChange>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PromptStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub change_type: FileChangeType,
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FileChangeType {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpConnectionConfig {
    pub agent_type: AgentType,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::HashMap<String, String>,
    pub working_directory: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentType {
    ClaudeCode,
    Gemini,
    Custom,
}