mod agent;
use agent_client_protocol::{AgentSideConnection, Client};
use anyhow::Result;
use tokio::{io, sync::mpsc, task};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::error;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::agent::CodexAgent;
use codex_core::config::{Config, ConfigOverrides};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Initialize tracing with env filter (RUST_LOG compatible).
    let filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new("info"))?;
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();

    let outgoing = io::stdout().compat_write();
    let incoming = io::stdin().compat();

    let local_set = task::LocalSet::new();
    local_set.run_until(async move {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (client_tx, mut client_rx) = mpsc::unbounded_channel();

        let config = Config::load_with_cli_overrides(vec![], ConfigOverrides::default())?;
        let agent = CodexAgent::with_config(tx, client_tx.clone(), config);
        let (conn, handle_io) = AgentSideConnection::new(agent, outgoing, incoming, |fut| {
            task::spawn_local(fut);
        });

        task::spawn_local(async move {
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Some((session_notification, tx)) => {
                                let result = conn.session_notification(session_notification).await;
                                if let Err(e) = result { error!(error = ?e, "failed to send session notification"); break; }
                                let _ = tx.send(());
                            }
                            None => break,
                        }
                    }
                    op = client_rx.recv() => {
                        match op {
                            Some(agent::ClientOp::RequestPermission(req, tx)) => {
                                let res = conn.request_permission(req).await;
                                let _ = tx.send(res);
                            }
                            None => break,
                        }
                    }
                }
            }
        });

        handle_io.await
    }).await
}
