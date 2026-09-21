//! Inventory hints feed the same read-only lifecycle restoration as first access.
//! A PVC/name/cache entry is never passed to storage as ownership evidence.
use super::*;
use std::collections::{BTreeSet, VecDeque};

pub(super) struct Discovery {
    queue: VecDeque<String>,
    refresh_at: tokio::time::Instant,
}
impl Default for Discovery {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            refresh_at: tokio::time::Instant::now(),
        }
    }
}
impl Discovery {
    pub(super) async fn poll(
        &mut self,
        state: &std::sync::Arc<AppState>,
        tasks: &mut RecoveryTasks,
    ) -> anyhow::Result<()> {
        if tasks.is_full() {
            return Ok(());
        }
        if self.queue.is_empty() && tokio::time::Instant::now() >= self.refresh_at {
            self.refresh_at = tokio::time::Instant::now() + Duration::from_secs(60);
            let runtime = state.runtime();
            let (containers, dev, prod) = tokio::time::timeout(SCAN_READ_TIMEOUT, async {
                tokio::try_join!(
                    runtime.list_containers(),
                    runtime.list_workspace_identifiers(&shared_types::ServiceType::UserappBuilder),
                    runtime.list_workspace_identifiers(&shared_types::ServiceType::Userapp)
                )
            })
            .await
            .map_err(|_| anyhow::anyhow!("Lifecycle discovery inventory timed out"))??;
            let mut ids: BTreeSet<String> = dev.into_iter().chain(prod).collect();
            for info in containers {
                if matches!(
                    info.service_type,
                    Some(
                        shared_types::ServiceType::Userapp
                            | shared_types::ServiceType::UserappBuilder
                    )
                ) && let Some(id) = info.app_id
                {
                    ids.insert(id);
                }
            }
            self.queue.extend(ids);
        }
        for _ in 0..2 {
            if tasks.is_full() {
                break;
            }
            let Some(app_id) = self.queue.pop_front() else {
                break;
            };
            let state = state.clone();
            tasks.push(format!("discovery:{app_id}"), async move {
                tokio::time::timeout(
                    Duration::from_secs(30),
                    state.app_service.discover_missing_identity(&app_id),
                )
                .await
                .map_err(|_| anyhow::anyhow!("Lifecycle restoration timed out"))??;
                Ok(())
            });
        }
        Ok(())
    }
}
