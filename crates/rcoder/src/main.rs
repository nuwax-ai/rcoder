use anyhow::Result;
use claude_integration::ClaudeCodeManager;
use http_server::run_server;
use project_manager::ProjectManager;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rcoder=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting rcoder - AI-powered development platform");

    // Initialize project manager
    let database_url = "sqlite:///./rcoder.db";
    let project_manager = Arc::new(
        ProjectManager::new(database_url)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to initialize project manager: {}", e))?,
    );

    // Initialize Claude Code manager
    let mut claude_manager = ClaudeCodeManager::new()
        .map_err(|e| anyhow::anyhow!("Failed to initialize Claude Code manager: {}", e))?;

    // Initialize Claude Code connection
    claude_manager.initialize()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to initialize Claude Code: {}", e))?;

    let claude_manager = Arc::new(claude_manager);

    // Start HTTP server
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .unwrap_or(3000);

    info!("Starting server on port {}", port);

    run_server(claude_manager, project_manager, port)
        .await
        .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

    info!("Server shutdown complete");
    Ok(())
}