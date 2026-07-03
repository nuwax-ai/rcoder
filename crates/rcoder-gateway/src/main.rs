use rcoder_gateway::{GatewayConfig, GatewayService};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = GatewayConfig::from_env();
    info!("[GATEWAY] config: {:?}", config);

    let service = GatewayService::new(config);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let ctrl_c = tokio::signal::ctrl_c();

    tokio::pin!(ctrl_c);

    tokio::select! {
        result = service.start(shutdown_rx) => {
            result
        }
        _ = ctrl_c => {
            info!("[GATEWAY] Ctrl+C received, shutting down");
            let _ = shutdown_tx.send(());
            Ok(())
        }
    }
}
