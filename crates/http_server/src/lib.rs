use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{get, post, delete, put},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tower_http::cors::{CorsLayer, Any};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub mod handlers;
pub mod middleware;
pub mod http_interface;
pub mod http_agent;

use handlers::*;
use middleware::*;
use http_interface::{HttpClaudeManager, HttpProjectManager};

pub struct AppState {
    pub claude_manager: Arc<HttpClaudeManager>,
    pub project_manager: Arc<HttpProjectManager>,
}

pub async fn create_app(
    claude_manager: Arc<HttpClaudeManager>,
    project_manager: Arc<HttpProjectManager>,
) -> Router {
    let state = AppState {
        claude_manager,
        project_manager,
    };

    Router::new()
        .route("/api/health", get(health_check))
        .route("/api/projects", get(list_projects).post(create_project))
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
    claude_manager: Arc<HttpClaudeManager>,
    project_manager: Arc<HttpProjectManager>,
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