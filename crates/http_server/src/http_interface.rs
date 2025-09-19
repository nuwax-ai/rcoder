use anyhow::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::http_agent::{HttpNativeAgent, TokenUsage};

/// HTTP友好的项目管理器
pub struct HttpProjectManager {
    projects: DashMap<Uuid, HttpProject>,
    working_dir: PathBuf,
}

/// HTTP项目包装器
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpProject {
    pub id: Uuid,
    pub path: PathBuf,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl HttpProjectManager {
    pub fn new(working_dir: PathBuf) -> Self {
        Self {
            projects: DashMap::new(),
            working_dir,
        }
    }

    /// 异步创建 HttpProjectManager 并加载现有项目
    pub async fn new_with_loading(working_dir: PathBuf) -> Result<Self> {
        let mut manager = Self::new(working_dir.clone());
        manager.load_existing_projects().await?;
        Ok(manager)
    }

    /// 从工作目录加载现有项目
    async fn load_existing_projects(&mut self) -> Result<()> {
        info!(
            "Loading existing projects from: {}",
            self.working_dir.display()
        );

        // 确保工作目录存在
        if !self.working_dir.exists() {
            tokio::fs::create_dir_all(&self.working_dir).await?;
            info!("Created working directory: {}", self.working_dir.display());
            return Ok(());
        }

        let mut entries = tokio::fs::read_dir(&self.working_dir).await?;
        let mut loaded_count = 0;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            // 只处理目录
            if !path.is_dir() {
                continue;
            }

            // 尝试将目录名解析为 UUID
            if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                if let Ok(project_id) = Uuid::parse_str(dir_name) {
                    // 检查项目元数据文件是否存在
                    let metadata_file = path.join("project_metadata.json");

                    let created_at = if metadata_file.exists() {
                        // 尝试从元数据文件读取创建时间
                        match Self::load_project_metadata_static(&metadata_file).await {
                            Ok(metadata) => metadata.created_at,
                            Err(e) => {
                                warn!("Failed to load metadata for project {}: {}, using directory modification time", project_id, e);
                                Self::get_directory_creation_time_static(&path).await?
                            }
                        }
                    } else {
                        // 使用目录修改时间作为创建时间
                        Self::get_directory_creation_time_static(&path).await?
                    };

                    let project = HttpProject {
                        id: project_id,
                        path: path.clone(),
                        created_at,
                    };

                    self.projects.insert(project_id, project);
                    loaded_count += 1;
                    debug!("Loaded project: {} from {}", project_id, path.display());
                }
            }
        }

        info!("Loaded {} existing projects", loaded_count);
        Ok(())
    }

    /// 加载项目元数据
    async fn load_project_metadata(&self, metadata_file: &PathBuf) -> Result<HttpProject> {
        let content = tokio::fs::read_to_string(metadata_file).await?;
        let project: HttpProject = serde_json::from_str(&content)?;
        Ok(project)
    }

    /// 静态方法：加载项目元数据
    async fn load_project_metadata_static(metadata_file: &PathBuf) -> Result<HttpProject> {
        let content = tokio::fs::read_to_string(metadata_file).await?;
        let project: HttpProject = serde_json::from_str(&content)?;
        Ok(project)
    }

    /// 获取目录的创建时间
    async fn get_directory_creation_time(
        &self,
        path: &PathBuf,
    ) -> Result<chrono::DateTime<chrono::Utc>> {
        let metadata = tokio::fs::metadata(path).await?;
        let system_time = metadata.modified()?;
        let datetime: chrono::DateTime<chrono::Utc> = system_time.into();
        Ok(datetime)
    }

    /// 静态方法：获取目录的创建时间
    async fn get_directory_creation_time_static(
        path: &PathBuf,
    ) -> Result<chrono::DateTime<chrono::Utc>> {
        let metadata = tokio::fs::metadata(path).await?;
        let system_time = metadata.modified()?;
        let datetime: chrono::DateTime<chrono::Utc> = system_time.into();
        Ok(datetime)
    }

    pub async fn create_project(&self, request: CreateProjectRequest) -> Result<HttpProject> {
        // 推荐做法如下，确保 project_id 既作为 Uuid 存储，也作为字符串用于路径拼接：
        let project_id = Uuid::now_v7();
        let project_path = self.working_dir.join(project_id.to_string());

        // 创建项目目录
        tokio::fs::create_dir_all(&project_path).await?;

        let project = HttpProject {
            id: project_id,
            path: project_path.clone(),
            created_at: chrono::Utc::now(),
        };

        self.projects.insert(project_id, project.clone());
        info!(
            "Created new project: {} at {}",
            project_id,
            project_path.display()
        );
        Ok(project)
    }

    pub async fn get_project(&self, project_id: Uuid) -> Option<HttpProject> {
        self.projects
            .get(&project_id)
            .map(|entry| entry.value().clone())
    }

    pub async fn list_projects(&self) -> Vec<HttpProject> {
        self.projects
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub async fn delete_project(&self, project_id: Uuid) -> Result<()> {
        self.projects.remove(&project_id);
        Ok(())
    }
}

/// HTTP友好的Claude Code管理器
pub struct HttpClaudeManager {
    agent: Arc<HttpNativeAgent>,
    project_sessions: DashMap<Uuid, Uuid>, // project_id -> session_id
}

impl HttpClaudeManager {
    pub async fn new() -> Result<Self> {
        Ok(Self {
            agent: Arc::new(HttpNativeAgent::new()),
            project_sessions: DashMap::new(),
        })
    }

    pub async fn send_prompt(
        &self,
        project_id: Uuid,
        prompt: String,
        working_dir: PathBuf,
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
            let project_path = working_dir.join(project_id.to_string());
            let new_session_id = self.agent.create_session(project_path).await?;

            // 保存会话映射
            self.project_sessions
                .write()
                .await
                .insert(project_id, new_session_id);
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
    pub user_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PromptRequest {
    pub user_id: String,
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
