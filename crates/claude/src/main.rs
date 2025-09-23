use std::path::PathBuf;

use agent_client_protocol::{AgentSideConnection, Client};
use anyhow::Result;
use tokio::{io, sync::mpsc, task};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<()> {
    info!("Starting Claude agent");
    Ok(())
}
