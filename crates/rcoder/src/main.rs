use anyhow::Result;
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
    let working_dir = std::env::current_dir()?.join("projects");
    tokio::fs::create_dir_all(&working_dir).await?;

 

    // Start HTTP server
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .unwrap_or(3000);

    info!("Starting server on port {}", port);

    // run_server(claude_manager, project_manager, port)
    //     .await
    //     .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

    info!("Server shutdown complete");
    Ok(())
}