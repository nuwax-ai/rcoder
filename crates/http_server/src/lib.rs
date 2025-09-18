use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{get, post, delete, put},
    Router,
};
use serde::{Deserialize, Serialize};
use shared_types::{CreateProjectRequest, PromptRequest, PromptResponse, Project};
use std::collections::HashMap;
use std::sync::Arc;
use tower_http::cors::{CorsLayer, Any};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub mod handlers;
pub mod middleware;

use handlers::*;
use middleware::*;

pub struct AppState {
    pub claude_manager: Arc<claude_integration::ClaudeCodeManager>,
    pub project_manager: Arc<project_manager::ProjectManager>,
}

pub async fn create_app(
    claude_manager: Arc<claude_integration::ClaudeCodeManager>,
    project_manager: Arc<project_manager::ProjectManager>,
) -> Router {
    let state = AppState {
        claude_manager,
        project_manager,
    };

    Router::new()
        .route("/api/health", get(health_check))
        .route("/api/projects", get(list_projects))
        .route("/api/projects/:id", get(get_project).put(update_project).delete(delete_project))
        .route("/api/projects/:id/stats", get(get_project_stats))
        .route("/api/projects/:id/files", get(get_project_files))
        .route("/api/prompts", post(send_prompt))
        .route("/api/prompts/:prompt_id", get(get_prompt_status))
        .layer(CorsLayer::permissive())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn run_server(
    claude_manager: Arc<claude_integration::ClaudeCodeManager>,
    project_manager: Arc<project_manager::ProjectManager>,
    port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    info!("Starting HTTP server on port {}", port);

    let app = create_app(claude_manager, project_manager).await;

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    info!("Server listening on {}", listener.local_addr()?);

    axum::serve(listener, app)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync + 'static>)
}