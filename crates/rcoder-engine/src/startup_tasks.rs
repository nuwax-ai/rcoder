//! Cooperative ownership of startup producers that may write workspace data.
use tokio_util::sync::CancellationToken;

pub struct StartupTaskHandles {
    stop: CancellationToken,
    batch: Option<tokio::task::JoinHandle<Result<(), String>>>,
    skills: Option<tokio::task::JoinHandle<Result<(), String>>>,
}

impl StartupTaskHandles {
    pub fn start(runtime: std::sync::Arc<dyn container_runtime_api::ContainerRuntime>) -> Self {
        let stop = CancellationToken::new();
        Self {
            batch: crate::batch_migrate::spawn_if_enabled(runtime, stop.clone()),
            skills: crate::skill_sync_reconciler::spawn_skill_sync_reconciler(stop.clone()),
            stop,
        }
    }

    /// Stop between complete work units; never drop a write in flight.
    pub fn stop(&self) {
        self.stop.cancel();
    }

    pub async fn drain(self, deadline: tokio::time::Instant) -> anyhow::Result<()> {
        self.stop.cancel();
        for (name, handle) in [("batch migration", self.batch), ("skill sync", self.skills)] {
            if let Some(handle) = handle {
                tokio::time::timeout_at(deadline, handle)
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "{name} drain timed out; workspace write exit remains unconfirmed"
                        )
                    })??
                    .map_err(anyhow::Error::msg)?;
            }
        }
        Ok(())
    }
}
