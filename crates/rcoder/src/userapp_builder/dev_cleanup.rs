//! Captured builder deletion. No cleanup target is rediscovered by app name.
use shared_types::{AppOperationLease, BuilderDeletionSnapshot, ProjectStore, UserappDevDeletion};
use std::sync::Arc;

pub struct UserappDevResourcesCleanup {
    runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
    projects: Arc<crate::storage::ProjectStoreBackend>,
    store: Arc<dyn shared_types::UserAppLifecycleStore>,
}
impl UserappDevResourcesCleanup {
    pub fn new(
        runtime: Arc<dyn container_runtime_api::ContainerRuntime>,
        projects: Arc<crate::storage::ProjectStoreBackend>,
        store: Arc<dyn shared_types::UserAppLifecycleStore>,
    ) -> Self {
        Self {
            runtime,
            projects,
            store,
        }
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
pub(super) struct BuilderOperation {
    lease: Option<Box<dyn AppOperationLease>>,
    mutating: bool,
}
impl BuilderOperation {
    pub(super) fn receipt(&self) -> Result<shared_types::UserAppOperationLeaseReceipt, String> {
        self.lease
            .as_ref()
            .and_then(|lease| lease.receipt())
            .ok_or_else(|| "Builder operation lease has no durable identity receipt".into())
    }

    pub(super) fn new(lease: Box<dyn AppOperationLease>) -> Self {
        Self {
            lease: Some(lease),
            mutating: false,
        }
    }

    pub(super) async fn finish_read_only(&mut self) -> Result<(), String> {
        if self.mutating {
            return Err("Builder operation has already submitted a mutation".into());
        }
        let lease = self
            .lease
            .take()
            .ok_or_else(|| "Builder operation lease is unavailable".to_owned())?;
        lease.release().await
    }

    pub(super) fn begin_external_mutation(&mut self) -> Result<(), String> {
        if self.lease.is_none() || self.mutating {
            return Err("Builder operation is not available for external mutation".into());
        }
        self.mutating = true;
        Ok(())
    }

    pub(super) async fn finish_external_mutation(&mut self) -> Result<(), String> {
        if !self.mutating {
            return Err("Builder external mutation has not started".into());
        }
        let lease = self
            .lease
            .take()
            .ok_or_else(|| "Builder external mutation lease is unavailable".to_owned())?;
        lease.release().await?;
        self.mutating = false;
        Ok(())
    }
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
        let mut snapshot = self
            .runtime
            .capture_builder_deletion(app_id)
            .await
            .map_err(|e| format!("capture builder deletion: {e}"))?;
        if let Some(resource) = snapshot.resources.iter().find(|resource| {
            matches!(
                resource.kind,
                shared_types::AppResourceKind::Container
                    | shared_types::AppResourceKind::StatefulSet
            )
        }) {
            snapshot.resource_binding = self
                .store
                .get_resource_binding(&shared_types::ServiceType::UserappBuilder, &resource.uid)
                .await
                .map_err(|error| format!("Read builder resource binding: {error}"))?;
            let app = self
                .store
                .get_application(app_id)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Builder lifecycle is missing".to_owned())?;
            let context = shared_types::UserAppExecutionContext {
                app_id: app_id.into(),
                user_id: app.user_id,
                lifecycle_id: app.lifecycle_id,
                operation_id: "capture-deletion".into(),
                executor_id: "reader".into(),
                request_fingerprint: "0".repeat(64),
            };
            let actual = self
                .runtime
                .capture_bound_builder_control(&context, snapshot.resource_binding.as_ref())
                .await
                .map_err(|error| format!("Verify builder deletion ownership: {error}"))?;
            if actual
                .workload
                .as_ref()
                .map(|workload| workload.uid.as_str())
                != Some(resource.uid.as_str())
            {
                return Err("Builder changed during deletion capture".into());
            }
        }
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
    async fn workspace_endpoint(
        &mut self,
        context: &shared_types::UserAppExecutionContext,
    ) -> Result<shared_types::UserAppBuilderWorkspaceEndpoint, String> {
        self.runtime
            .inspect_builder_workspace(&self.snapshot, context)
            .await
            .map_err(|error| format!("Inspect captured builder workspace: {error}"))
    }

    fn begin_external_mutation(&mut self) -> Result<(), String> {
        self.operation.begin_external_mutation()
    }

    async fn finish_external_mutation(&mut self) -> Result<(), String> {
        self.operation.finish_external_mutation().await
    }

    fn receipt(&self) -> shared_types::UserappDevDeletionReceipt {
        shared_types::UserappDevDeletionReceipt {
            runtime: self.snapshot.clone(),
            registry: self
                .registry_identity
                .as_ref()
                .map(
                    |(generation, container_id)| shared_types::BuilderRegistryIdentity {
                        generation: generation.clone(),
                        container_id: container_id.clone(),
                    },
                ),
        }
    }

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
        let mut operation = BuilderOperation {
            lease: Some(Box::new(Lease(send))),
            mutating: false,
        };
        operation
            .begin_external_mutation()
            .expect("begin external write");
        drop(operation);
        assert!(receive.await.is_err());
    }

    #[tokio::test]
    async fn confirmed_external_write_releases_once_and_cannot_be_reused() {
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut operation = BuilderOperation {
            lease: Some(Box::new(Lease(send))),
            mutating: false,
        };
        assert!(operation.finish_external_mutation().await.is_err());
        operation.begin_external_mutation().expect("begin");
        assert!(
            operation.begin_external_mutation().is_err(),
            "cannot overlap writes"
        );
        operation
            .finish_external_mutation()
            .await
            .expect("release after confirmation");
        receive.await.expect("lease released");
        assert!(!operation.mutating);
        assert!(operation.begin_external_mutation().is_err());
        assert!(operation.finish_external_mutation().await.is_err());
    }

    #[tokio::test]
    async fn failed_external_lease_release_preserves_uncertain_state() {
        let (send, receive) = tokio::sync::oneshot::channel();
        drop(receive);
        let mut operation = BuilderOperation {
            lease: Some(Box::new(Lease(send))),
            mutating: false,
        };
        operation.begin_external_mutation().expect("begin");
        assert!(operation.finish_external_mutation().await.is_err());
        assert!(
            operation.mutating,
            "release failure cannot be treated as read-only cleanup"
        );
    }
}
