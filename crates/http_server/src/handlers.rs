use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    Json as AxumJson,
};
use serde::{Deserialize, Serialize};
use shared_types::{CreateProjectRequest, PromptRequest, PromptResponse, Project};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use std::sync::Arc;

use crate::AppState;

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
) -> Result<Json<Vec<Project>>, StatusCode> {
    debug!("Listing projects with query: {:?}", params);

    let projects = if let Some(search) = &params.search {
        // In a real implementation, you would add search functionality
        state.project_manager.list_projects().await
            .map_err(|e| {
                error!("Failed to list projects: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })
    } else {
        state.project_manager.list_projects().await
            .map_err(|e| {
                error!("Failed to list projects: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })
    };

    projects.map(Json)
}

pub async fn create_project(
    State(state): State<AppState>,
    AxumJson(request): AxumJson<CreateProjectRequest>,
) -> Result<Json<Project>, StatusCode> {
    info!("Creating project: {}", request.name);

    // Create project in project manager
    let project = state.project_manager.create_project(request).await
        .map_err(|e| {
            error!("Failed to create project: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Initialize with Claude Code
    match state.claude_manager.create_project(
        &project.name,
        project.description.as_deref(),
        request.template.as_deref(),
        Some(&project.path),
    ).await {
        Ok(claude_project_id) => {
            info!("Project initialized with Claude Code: {}", claude_project_id);
        }
        Err(e) => {
            error!("Failed to initialize project with Claude Code: {}", e);
            // We still return the project since it was created successfully
            warn!("Project created but Claude Code initialization failed");
        }
    }

    Ok(Json(project))
}

pub async fn get_project(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Project>, StatusCode> {
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
    AxumJson(request): AxumJson<UpdateProjectRequest>,
) -> Result<Json<Project>, StatusCode> {
    info!("Updating project: {}", project_id);

    let updates = project_manager::ProjectUpdate {
        name: request.name,
        description: request.description,
    };

    let project = state.project_manager.update_project(project_id, &updates).await
        .map_err(|e| {
            error!("Failed to update project: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(project))
}

pub async fn delete_project(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> Result<StatusCode, StatusCode> {
    info!("Deleting project: {}", project_id);

    // First delete from Claude Code
    if let Err(e) = state.claude_manager.delete_project(project_id).await {
        error!("Failed to delete project from Claude Code: {}", e);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    // Then delete from project manager
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

    let stats = state.project_manager.get_project_stats(project_id).await
        .map_err(|e| {
            error!("Failed to get project stats: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(serde_json::to_value(stats).unwrap()))
}

pub async fn get_project_files(
    State(state): State<AppState>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Vec<std::path::PathBuf>>, StatusCode> {
    debug!("Getting project files: {}", project_id);

    let files = state.project_manager.get_project_files(project_id).await
        .map_err(|e| {
            error!("Failed to get project files: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(files))
}

pub async fn send_prompt(
    State(state): State<AppState>,
    AxumJson(request): AxumJson<PromptRequest>,
) -> Result<Json<PromptResponse>, StatusCode> {
    info!("Processing prompt: {}", request.prompt);

    let project_id = if let Some(id) = request.project_id {
        // Use existing project
        let project = state.project_manager.get_project(id).await
            .ok_or(StatusCode::NOT_FOUND)?;
        id
    } else {
        // Auto-create project based on prompt
        let auto_create = request.auto_create.unwrap_or(true);
        if !auto_create {
            return Err(StatusCode::BAD_REQUEST);
        }

        let project_name = extract_project_name_from_prompt(&request.prompt)
            .unwrap_or_else(|| "auto-project".to_string());

        let project = state.project_manager.create_project(shared_types::CreateProjectRequest {
            name: project_name.clone(),
            description: Some(format!("Auto-created project for prompt: {}", &request.prompt[..100.min(request.prompt.len())])),
            template: None,
            path: None,
        }).await.map_err(|e| {
            error!("Failed to auto-create project: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        // Initialize with Claude Code
        match state.claude_manager.create_project(
            &project.name,
            project.description.as_deref(),
            None, // Let Claude Code detect the template from prompt
            Some(&project.path),
        ).await {
            Ok(claude_project_id) => {
                info!("Auto-created project initialized with Claude Code: {}", claude_project_id);
            }
            Err(e) => {
                error!("Failed to initialize auto-created project with Claude Code: {}", e);
                warn!("Auto-created project but Claude Code initialization failed");
            }
        }

        project.id
    };

    // Send prompt to Claude Code
    let response = state.claude_manager.process_prompt(
        project_id,
        &request.prompt,
        request.context.as_ref().map(|c| c.files.clone()),
    ).await
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
    Path((project_id, prompt_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<PromptResponse>, StatusCode> {
    debug!("Getting prompt status for project {}: {}", project_id, prompt_id);

    // In a real implementation, you would fetch the prompt status from the database
    // For now, we'll return a placeholder
    Err(StatusCode::NOT_IMPLEMENTED)
}

#[derive(Serialize)]
pub struct TemplateResponse {
    pub name: String,
    pub description: String,
    pub language: String,
    pub files: Vec<String>,
}

pub async fn list_templates(
    State(state): State<AppState>,
) -> Result<Json<Vec<TemplateResponse>>, StatusCode> {
    debug!("Listing templates");

    let templates = state.claude_manager.template_manager.list_templates();
    let response = templates.into_iter().map(|t| TemplateResponse {
        name: t.name.clone(),
        description: t.description.clone(),
        language: t.language.clone(),
        files: t.files.keys().cloned().collect(),
    }).collect();

    Ok(Json(response))
}

pub async fn get_template(
    State(state): State<AppState>,
    Path(template_name): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    debug!("Getting template: {}", template_name);

    let template = state.claude_manager.template_manager.get_template(&template_name)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::to_value(template).unwrap()))
}

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