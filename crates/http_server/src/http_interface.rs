use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::http_agent::{HttpNativeAgent, PromptResponse, TokenUsage};

/// HTTP友好的项目管理器
pub struct HttpProjectManager {
    projects: Arc<RwLock<HashMap<Uuid, HttpProject>>>,
    working_dir: PathBuf,
}

/// HTTP项目包装器
pub struct HttpProject {
    pub id: Uuid,
    pub name: String,
    pub path: PathBuf,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl HttpProjectManager {
    pub fn new(working_dir: PathBuf) -> Self {
        Self {
            projects: Arc::new(RwLock::new(HashMap::new())),
            working_dir,
        }
    }

    pub async fn create_project(&self, request: CreateProjectRequest) -> Result<HttpProject> {
        let project_id = Uuid::new_v4();
        let project_path = self.working_dir.join(&request.name);

        // 创建项目目录
        tokio::fs::create_dir_all(&project_path).await?;

        let project = HttpProject {
            id: project_id,
            name: request.name.clone(),
            path: project_path.clone(),
            created_at: chrono::Utc::now(),
        };

        self.projects.write().await.insert(project_id, project.clone());
        Ok(project)
    }

    pub async fn get_project(&self, project_id: Uuid) -> Option<HttpProject> {
        self.projects.read().await.get(&project_id).cloned()
    }

    pub async fn list_projects(&self) -> Vec<HttpProject> {
        self.projects.read().await.values().cloned().collect()
    }

    pub async fn delete_project(&self, project_id: Uuid) -> Result<()> {
        self.projects.write().await.remove(&project_id);
        Ok(())
    }
}

/// HTTP友好的Claude Code管理器
pub struct HttpClaudeManager {
    agent: Arc<HttpNativeAgent>,
    project_sessions: Arc<RwLock<HashMap<Uuid, Uuid>>>, // project_id -> session_id
}

impl HttpClaudeManager {
    pub async fn new() -> Result<Self> {
        Ok(Self {
            agent: Arc::new(HttpNativeAgent::new()),
            project_sessions: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub async fn send_prompt(
        &self,
        project_id: Uuid,
        prompt: String,
    ) -> Result<PromptResponse> {
        debug!("Sending prompt for project {}: {}", project_id, prompt);

        // 检查是否已有会话
        let session_id = {
            let sessions = self.project_sessions.read().await;
            sessions.get(&project_id).cloned()
        };

        let session_id = if let Some(existing_session_id) = session_id {
            // 使用现有会话
            existing_session_id
        } else {
            // 创建新会话
            let project_path = PathBuf::from(format!("./projects/{}", project_id));
            let new_session_id = self.agent.create_session(project_path).await?;

            // 保存会话映射
            self.project_sessions.write().await.insert(project_id, new_session_id);
            new_session_id
        };

        // 发送prompt到Claude Code
        let response = self.agent.send_prompt(session_id, prompt).await?;

        Ok(response)
    }

    pub async fn get_prompt_status(&self, session_id: Uuid) -> Result<PromptResponse> {
        debug!("Getting status for session: {}", session_id);

        // 查询会话状态
        if let Some(_session) = self.agent.get_session(session_id).await {
            Ok(PromptResponse {
                session_id: session_id.to_string(),
                response: "Session is active".to_string(),
                status: "active".to_string(),
                files_modified: vec![],
                token_usage: None,
            })
        } else {
            Err(anyhow::anyhow!("Session not found: {}", session_id))
        }
    }

    pub async fn close_project_session(&self, project_id: Uuid) -> Result<()> {
        debug!("Closing session for project: {}", project_id);

        if let Some(session_id) = self.project_sessions.write().await.remove(&project_id) {
            self.agent.close_session(session_id).await?;
        }

        Ok(())
    }

    pub async fn list_active_sessions(&self) -> Vec<crate::http_agent::SessionStatus> {
        let mut sessions = Vec::new();

        for session_id in self.project_sessions.read().await.values() {
            if let Ok(status) = self.agent.get_session_status(*session_id).await {
                sessions.push(status);
            }
        }

        sessions
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
    pub description: Option<String>,
    pub template: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PromptRequest {
    pub project_id: Option<Uuid>,
    pub prompt: String,
    pub context: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PromptResponse {
    pub session_id: String,
    pub response: String,
    pub status: String,
    pub files_modified: Vec<String>,
    pub token_usage: Option<TokenUsage>,
}

