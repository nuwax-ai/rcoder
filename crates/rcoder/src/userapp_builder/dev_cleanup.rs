//! Captured builder deletion. No cleanup target is rediscovered by app name.
use shared_types::{AppOperationLease, BuilderDeletionSnapshot, ProjectStore, UserappDevDeletion};
use std::sync::Arc;

pub struct UserappDevResourcesCleanup {
    runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
    projects: Arc<crate::storage::ProjectStoreBackend>,
}
impl UserappDevResourcesCleanup {
    pub fn new(
        runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
        projects: Arc<crate::storage::ProjectStoreBackend>,
    ) -> Self {
        Self { runtime, projects }
    }
}
struct CapturedDeletion {
    runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
    projects: Arc<crate::storage::ProjectStoreBackend>,
    snapshot: BuilderDeletionSnapshot,
    registry_identity: Option<(String, String)>,
    operation: BuilderOperation,
    _local: tokio::sync::OwnedMutexGuard<()>,
}
// Read-only capture may be abandoned safely. Once deletion starts, an uncertain
// Kubernetes operation retains its lease instead of admitting a competing writer.
struct BuilderOperation {
    lease: Option<Box<dyn AppOperationLease>>,
    mutating: bool,
}
impl Drop for BuilderOperation {
    fn drop(&mut self) {
        if self.mutating {
            return;
        }
        if let Some(lease) = self.lease.take() {
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn(async move {
                    if let Err(error) = lease.release().await {
                        tracing::error!(%error, "Failed to release read-only builder operation");
                    }
                });
                }
                Err(error) => {
                    tracing::error!(%error, "Builder operation release requires an active runtime")
                }
            }
        }
    }
}
#[async_trait::async_trait]
impl shared_types::UserappDevCleanup for UserappDevResourcesCleanup {
    async fn capture(&self, app_id: &str) -> Result<Box<dyn UserappDevDeletion>, String> {
        let local = super::lifecycle::acquire(app_id).await;
        let operation = self
            .runtime
            .acquire_builder_operation(app_id)
            .await
            .map_err(|e| format!("acquire builder deletion: {e}"))?;
        let operation = BuilderOperation {
            lease: Some(operation),
            mutating: false,
        };
        let snapshot = self
            .runtime
            .capture_builder_deletion(app_id)
            .await
            .map_err(|e| format!("capture builder deletion: {e}"))?;
        let registry_identity = self
            .projects
            .get(app_id)
            .map(|project| {
                if project.service_type() != Some(shared_types::ServiceType::UserappBuilder) {
                    return Err("registration belongs to another service family".to_string());
                }
                let container = project.container_info().ok_or_else(|| {
                    "builder registration has no physical container identity".to_string()
                })?;
                if container.container_id.is_empty() {
                    return Err("builder registration has empty container ID".into());
                }
                Ok((
                    project.persistence_identity().generation.clone(),
                    container.container_id,
                ))
            })
            .transpose()?;
        Ok(Box::new(CapturedDeletion {
            runtime: self.runtime.clone(),
            projects: self.projects.clone(),
            snapshot,
            registry_identity,
            operation,
            _local: local,
        }))
    }
}
#[async_trait::async_trait]
impl UserappDevDeletion for CapturedDeletion {
    async fn cleanup(self: Box<Self>) -> Result<(), String> {
        // Once accepted, retain both leases until every blocking filesystem action
        // finishes even if the HTTP caller disconnects or cancels its future.
        tokio::spawn(async move { self.execute().await })
            .await
            .map_err(|e| format!("builder cleanup task failed: {e}"))?
    }
}
impl CapturedDeletion {
    async fn execute(mut self: Box<Self>) -> Result<(), String> {
        self.operation.mutating =
            !self.snapshot.resources.is_empty() || self.snapshot.docker_bind_cleanup;
        self.runtime
            .delete_builder_snapshot(&self.snapshot)
            .await
            .map_err(|e| format!("delete captured builder: {e}"))?;
        if self.snapshot.docker_bind_cleanup {
            remove_bind_directories(&self.snapshot.app_id).await?;
        }
        match &self.registry_identity {
            Some((generation, container_id)) => {
                if !self
                    .projects
                    .remove_durable_if_container_identity(
                        &self.snapshot.app_id,
                        generation,
                        container_id,
                    )
                    .await
                    .map_err(|e| format!("remove captured builder registration: {e}"))?
                    && self.projects.get(&self.snapshot.app_id).is_some()
                {
                    return Err("builder registration changed during cleanup".into());
                }
            }
            None if self.projects.get(&self.snapshot.app_id).is_some() => {
                return Err("builder registration appeared after deletion capture".into());
            }
            None => {}
        }
        crate::userapp_forward::invalidate_probe_cache(&self.snapshot.app_id);
        if let Some(operation) = self.operation.lease.take() {
            operation.release().await?;
        }
        tracing::info!(app_id = %self.snapshot.app_id, operation_id = %self.snapshot.operation_id, "Captured builder resources deleted");
        Ok(())
    }
}
async fn remove_if_present(path: &std::path::Path) -> Result<(), String> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "remove captured builder bind directory {}: {e}",
            path.display()
        )),
    }
}
async fn remove_bind_directories(app_id: &str) -> Result<(), String> {
    let anchor = std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT);
    match tokio::fs::read_dir(anchor.join("dev")).await {
        Ok(mut entries) => {
            while let Some(user) = entries
                .next_entry()
                .await
                .map_err(|e| format!("read builder owners: {e}"))?
            {
                if !user
                    .file_type()
                    .await
                    .map_err(|e| format!("stat builder owner: {e}"))?
                    .is_dir()
                {
                    continue;
                }
                for suffix in shared_types::paths::userapp_dev_app_suffixes(app_id) {
                    remove_if_present(&user.path().join(suffix)).await?;
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("read builder bind root: {e}")),
    }
    remove_if_present(&anchor.join(app_id)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Lease(tokio::sync::oneshot::Sender<()>);
    #[async_trait::async_trait]
    impl AppOperationLease for Lease {
        async fn release(self: Box<Self>) -> Result<(), String> {
            self.0.send(()).map_err(|_| "receiver gone".into())
        }
    }
    #[tokio::test]
    async fn abandoned_read_only_capture_releases_lease() {
        let (send, receive) = tokio::sync::oneshot::channel();
        drop(BuilderOperation {
            lease: Some(Box::new(Lease(send))),
            mutating: false,
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), receive)
            .await
            .unwrap()
            .unwrap();
    }
    #[tokio::test]
    async fn uncertain_mutation_does_not_release_distributed_lease() {
        let (send, receive) = tokio::sync::oneshot::channel();
        drop(BuilderOperation {
            lease: Some(Box::new(Lease(send))),
            mutating: true,
        });
        assert!(receive.await.is_err());
    }
}
