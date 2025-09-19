use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use std::sync::Arc;

use crate::AppState;
use crate::http_interface::{CreateProjectRequest, PromptRequest, PromptResponse, HttpProject};

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

pub async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "healthy".to_string(),
        version: "1.0.0".to_string(),
        timestamp: chrono::Utc::now(),
    })
}

#[derive(Deserialize)]
pub struct ListProjectsQuery {
    pub search: Option<String>,
    pub page: Option<u32>,
    pub limit: Option<u32>,
}

pub async fn list_projects(
    State(state): State<AppState>,
    Query(params): Query<ListProjectsQuery>,
) -> Result<Json<Vec<HttpProject>>, StatusCode> {
    debug!("Listing projects with query: {:?}", params);

    let projects = state.project_manager.list_projects().await;
    Ok(Json(projects))
}

pub async fn create_project(
    State(state): State<AppState>,
    Json(request): Json<CreateProjectRequest>,
) -> Result<Json<HttpProject>, StatusCode> {
    info!("Creating project: {}", request.name);

    // Create project in project manager
    let project = state.project_manager.create_project(request).await
        .map_err(|e| {
            error!("Failed to create project: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(project))
}

pub async fn get_project(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<HttpProject>, StatusCode> {
    debug!("Getting project: {}", project_id);

    let project = state.project_manager.get_project(project_id).await
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(project))
}

#[derive(Deserialize)]
pub struct UpdateProjectRequest {
    pub name: Option<String>,
    pub description: Option<String>,
}

pub async fn update_project(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
    Json(request): Json<UpdateProjectRequest>,
) -> Result<Json<HttpProject>, StatusCode> {
    info!("Updating project: {}", project_id);

    // TODO: 实现项目更新逻辑
    let project = state.project_manager.get_project(project_id).await
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(project))
}

pub async fn delete_project(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    info!("Deleting project: {}", project_id);

    state.project_manager.delete_project(project_id).await
        .map_err(|e| {
            error!("Failed to delete project: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_project_stats(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    debug!("Getting project stats: {}", project_id);

    // TODO: 实现项目统计
    let stats = serde_json::json!({
        "project_id": project_id,
        "file_count": 0,
        "last_updated": chrono::Utc::now()
    });

    Ok(Json(stats))
}

pub async fn get_project_files(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Vec<std::path::PathBuf>>, StatusCode> {
    debug!("Getting project files: {}", project_id);

    // TODO: 实现文件列表获取
    Ok(Json(vec![]))
}

pub async fn send_prompt(
    State(state): State<AppState>,
    Json(request): Json<PromptRequest>,
) -> Result<Json<PromptResponse>, StatusCode> {
    info!("Processing prompt: {}", request.prompt);

    let project_id = if let Some(id) = request.project_id {
        // Use existing project
        let _project = state.project_manager.get_project(id).await
            .ok_or(StatusCode::NOT_FOUND)?;
        id
    } else {
        // Auto-create project based on prompt
        let project_name = extract_project_name_from_prompt(&request.prompt)
            .unwrap_or_else(|| "auto-project".to_string());

        let create_request = CreateProjectRequest {
            name: project_name.clone(),
            description: Some(format!("Auto-created project for prompt: {}", &request.prompt[..100.min(request.prompt.len())])),
            template: None,
        };

        let project = state.project_manager.create_project(create_request).await
            .map_err(|e| {
                error!("Failed to auto-create project: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

        project.id
    };

    // Send prompt to Claude Code
    let response = state.claude_manager.send_prompt(project_id, request.prompt).await
        .map_err(|e| {
            error!("Failed to process prompt: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(response))
}

fn extract_project_name_from_prompt(prompt: &str) -> Option<String> {
    // Simple project name extraction from prompt
    let prompt_lower = prompt.to_lowercase();

    // Look for patterns like "create a project called X" or "make a X project"
    if let Some(start) = prompt_lower.find("project called ") {
        let start_idx = start + "project called ".len();
        if let Some(end) = prompt[start_idx..].find(|c| c == ' ' || c == ',' || c == '.') {
            return Some(prompt[start_idx..start_idx + end].to_string());
        }
    }

    if let Some(start) = prompt_lower.find("create a ") {
        let start_idx = start + "create a ".len();
        if let Some(end) = prompt[start_idx..].find(" project") {
            return Some(prompt[start_idx..start_idx + end].to_string());
        }
    }

    if let Some(start) = prompt_lower.find("make a ") {
        let start_idx = start + "make a ".len();
        if let Some(end) = prompt[start_idx..].find(" project") {
            return Some(prompt[start_idx..start_idx + end].to_string());
        }
    }

    // Default project name based on timestamp
    Some(format!("project-{}", chrono::Utc::now().timestamp()))
}

fn extract_template_from_prompt(prompt: &str) -> Option<String> {
    let prompt_lower = prompt.to_lowercase();

    if prompt_lower.contains("rust") || prompt_lower.contains("api") {
        Some("rust-web-api".to_string())
    } else if prompt_lower.contains("react") || prompt_lower.contains("frontend") || prompt_lower.contains("web") {
        Some("react-frontend".to_string())
    } else if prompt_lower.contains("python") || prompt_lower.contains("cli") {
        Some("python-cli".to_string())
    } else {
        None
    }
}

pub async fn get_prompt_status(
    State(state): State<AppState>,
    Path(prompt_id): Path<Uuid>,
) -> Result<Json<PromptResponse>, StatusCode> {
    debug!("Getting prompt status: {}", prompt_id);

    let response = state.claude_manager.get_prompt_status(prompt_id).await
        .map_err(|e| {
            error!("Failed to get prompt status: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(response))
}

#[derive(Serialize)]
pub struct TemplateResponse {
    pub name: String,
    pub description: String,
    pub language: String,
    pub files: Vec<String>,
}

// Template functions removed - templates will be handled by MCP tools

#[derive(Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl ErrorResponse {
    pub fn new(error: String, message: String) -> Self {
        Self {
            error,
            message,
            timestamp: chrono::Utc::now(),
        }
    }
}