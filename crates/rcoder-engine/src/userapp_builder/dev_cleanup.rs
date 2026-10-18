//! Captured builder deletion. No cleanup target is rediscovered by app name.
use shared_types::storage_contents::{CapturedStorageDirectory, StorageDirectoryLease};
use shared_types::{AppOperationLease, BuilderDeletionSnapshot, ProjectStore, UserappDevDeletion};
use std::sync::Arc;

#[cfg(test)]
mod retirement_tests;

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
    store: Arc<dyn shared_types::UserAppLifecycleStore>,
    snapshot: BuilderDeletionSnapshot,
    lifecycle_id: String,
    receipt: shared_types::UserappDevDeletionReceipt,
    retirement_target: Option<shared_types::BuilderControlTarget>,
    execution_context: Option<shared_types::UserAppExecutionContext>,
    operation: BuilderOperation,
    /// 协作者实例（owner 实例之外的全部 `{user_id}-{app_id}` 实例）：
    /// (复合 identifier, 捕获快照)。cleanup 时与主快照一并删除。
    extra_instances: Vec<(String, BuilderDeletionSnapshot)>,
    /// 协作者实例的 operation 锁（capture 时获取，cleanup 完成后释放——
    /// 防清扫期间并发 ensure 重建实例）。
    extra_leases: Vec<BuilderOperation>,
    directories: Vec<CapturedStorageDirectory>,
    _local: Option<tokio::sync::OwnedMutexGuard<()>>,
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
    async fn capture_for_execution(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> Result<Box<dyn UserappDevDeletion>, String> {
        self.store
            .check_business_execution(context)
            .await
            .map_err(|error| error.to_string())?;
        let local = super::lifecycle::acquire(&context.app_id).await;
        let lease = self
            .runtime
            .acquire_builder_operation(&context.app_id)
            .await
            .map_err(|error| error.to_string())?;
        self.capture_owned(&context.app_id, lease, Some(local), Some(context.clone()))
            .await
    }

    async fn capture_with_lease_for_execution(
        &self,
        context: &shared_types::UserAppExecutionContext,
        lease: Box<dyn AppOperationLease>,
    ) -> Result<Box<dyn UserappDevDeletion>, String> {
        self.capture_owned(&context.app_id, lease, None, Some(context.clone()))
            .await
    }

    async fn inspect_captured(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserappDevDeletionReceipt,
    ) -> Result<shared_types::DeletionInspection, String> {
        use shared_types::DeletionInspection as Observation;
        let mut remaining_resources = match self
            .runtime
            .inspect_builder_deletion(context, receipt.lease.as_ref(), &receipt.runtime)
            .await
            .map_err(|error| error.to_string())?
        {
            Observation::ConfirmedQuiescent {
                remaining_resources,
            } => remaining_resources,
            other => return Ok(other),
        };
        for collaborator in &receipt.collaborators {
            let mut collaborator_context = context.clone();
            collaborator_context.app_id = collaborator.runtime.app_id.clone();
            match self
                .runtime
                .inspect_builder_deletion(
                    &collaborator_context,
                    collaborator.lease.as_ref(),
                    &collaborator.runtime,
                )
                .await
                .map_err(|error| error.to_string())?
            {
                Observation::ConfirmedQuiescent {
                    remaining_resources: resources,
                } => remaining_resources.extend(resources),
                other => return Ok(other),
            }
        }
        if receipt.runtime.docker_bind_cleanup {
            let Some(directories) = &receipt.directories else {
                return Ok(Observation::Unknown(
                    "Historical Docker deletion lacks directory identity witnesses".into(),
                ));
            };
            // The original builder FD was proved inactive above. No background
            // cleanup can remain active: CapturedDeletion retains it through all
            // detached filesystem work. Observe paths only; recovery preserves data.
            for directory in directories {
                shared_types::storage_contents::retire_captured_directory_deletion(
                    directory,
                    &receipt.runtime.operation_id,
                )
                .await
                .map_err(|error| {
                    format!(
                        "Retire development directory deletion {}: {error}",
                        directory.path.display()
                    )
                })?;
                let current = StorageDirectoryLease::capture(&directory.path)
                    .await
                    .map_err(|error| {
                        format!(
                            "Inspect captured development directory {}: {error}",
                            directory.path.display()
                        )
                    })?;
                if let Some(identity) = current.receipt.identity {
                    remaining_resources.push(shared_types::AppResourceIdentity {
                        kind: shared_types::AppResourceKind::HostPath,
                        name: directory.path.to_string_lossy().into_owned(),
                        uid: format!("{}:{}", identity.device, identity.inode),
                        resource_version: None,
                    });
                }
            }
        }
        Ok(Observation::ConfirmedQuiescent {
            remaining_resources,
        })
    }
    async fn capture(&self, app_id: &str) -> Result<Box<dyn UserappDevDeletion>, String> {
        let local = super::lifecycle::acquire(app_id).await;
        let lease = self
            .runtime
            .acquire_builder_operation(app_id)
            .await
            .map_err(|error| format!("acquire builder deletion: {error}"))?;
        self.capture_owned(app_id, lease, Some(local), None).await
    }
    async fn capture_with_lease(
        &self,
        app_id: &str,
        lease: Box<dyn AppOperationLease>,
    ) -> Result<Box<dyn UserappDevDeletion>, String> {
        // The caller already holds the lifecycle admission and physical lease.
        // Do not invert ensure's local -> physical lock order by taking local now.
        self.capture_owned(app_id, lease, None, None).await
    }
}
impl UserappDevResourcesCleanup {
    async fn capture_owned(
        &self,
        app_id: &str,
        lease: Box<dyn AppOperationLease>,
        local: Option<tokio::sync::OwnedMutexGuard<()>>,
        execution_context: Option<shared_types::UserAppExecutionContext>,
    ) -> Result<Box<dyn UserappDevDeletion>, String> {
        // Install read-only cleanup before any fallible observation so failed
        // capture releases its own lease; a borrowed lease leaves its owner intact.
        let operation = BuilderOperation::new(lease);
        let app = self
            .store
            .get_application(app_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Builder lifecycle is missing".to_owned())?;
        // 应用共享：instance == 纯 app_id（正式 evidence 链覆盖 lifecycle
        // 持有的唯一 builder；协作者实例模型已随用户绑定移除）。
        let owner_instance = app_id.to_string();
        if let Some(context) = &execution_context {
            if context.app_id != app_id || context.lifecycle_id != app.lifecycle_id {
                return Err(
                    "Builder deletion execution belongs to another application lifecycle".into(),
                );
            }
            self.store
                .check_business_execution(context)
                .await
                .map_err(|error| error.to_string())?;
        }
        let mut retirement_target = None;
        let mut snapshot = self
            .runtime
            .capture_builder_deletion(&owner_instance)
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
            let context = execution_context.clone().unwrap_or_else(|| {
                shared_types::UserAppExecutionContext {
                    app_id: owner_instance.clone(),
                    lifecycle_id: app.lifecycle_id.clone(),
                    operation_id: "capture-deletion".into(),
                    executor_id: "reader".into(),
                    request_fingerprint: "0".repeat(64),
                }
            });
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
            if actual.context != context || actual.resource_binding != snapshot.resource_binding {
                return Err("Builder deletion control identity changed during capture".into());
            }
            retirement_target = Some(actual);
        }
        // 协作者实例清点（label/名字聚合；排除 owner 实例）：各实例持有
        // operation 锁（防清扫期间并发 ensure 重建）+ 捕获快照，cleanup 时删除。
        let mut extra_instances = Vec::new();
        let mut extra_leases = Vec::new();
        for instance in self
            .runtime
            .find_builder_instances(app_id)
            .await
            .map_err(|e| format!("enumerate builder instances: {e}"))?
        {
            if instance == owner_instance {
                continue;
            }
            let lease = self
                .runtime
                .acquire_builder_operation(&instance)
                .await
                .map_err(|e| format!("acquire builder instance deletion ({instance}): {e}"))?;
            let lease = BuilderOperation::new(lease);
            let extra_snapshot = self
                .runtime
                .capture_builder_deletion(&instance)
                .await
                .map_err(|e| format!("capture builder instance deletion ({instance}): {e}"))?;
            extra_leases.push(lease);
            extra_instances.push((instance, extra_snapshot));
        }
        // Freeze the authoritative registry hint while the original physical
        // guard is held. A peer's old row can be absent from this replica's
        // mirror; that cache miss must not become a new physical identity.
        let registry_identity = self
            .projects
            .capture_builder_registry_identity(&owner_instance)
            .await
            .map_err(|error| format!("Capture builder registry identity: {error:#}"))?;
        let directories = if snapshot.docker_bind_cleanup {
            capture_bind_directories(app_id).await?
        } else {
            Vec::new()
        };
        let receipt = shared_types::UserappDevDeletionReceipt {
            runtime: snapshot.clone(),
            registry: registry_identity,
            lease: operation.lease.as_ref().and_then(|lease| lease.receipt()),
            collaborators: extra_instances
                .iter()
                .zip(&extra_leases)
                .map(
                    |((_, runtime), lease)| shared_types::BuilderCollaboratorDeletionReceipt {
                        runtime: runtime.clone(),
                        lease: lease.lease.as_ref().and_then(|lease| lease.receipt()),
                        registry: None,
                    },
                )
                .collect(),
            directories: snapshot.docker_bind_cleanup.then(|| directories.clone()),
        };
        Ok(Box::new(CapturedDeletion {
            runtime: self.runtime.clone(),
            projects: self.projects.clone(),
            store: self.store.clone(),
            snapshot,
            lifecycle_id: app.lifecycle_id,
            receipt,
            retirement_target,
            execution_context,
            operation,
            extra_instances,
            extra_leases,
            directories,
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
        self.receipt.clone()
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
    async fn store_execution_is_current(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> Result<(), String> {
        self.store
            .check_business_execution(context)
            .await
            .map_err(|error| format!("Builder deletion execution changed: {error}"))?;
        let operation = self
            .store
            .get_operation(&context.app_id, &context.operation_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Original builder deletion operation is missing".to_string())?;
        let original = shared_types::captured_builder_deletion_for_execution(&operation, context)?;
        if original != self.receipt {
            return Err("Original builder deletion capture changed".into());
        }
        Ok(())
    }

    async fn execute(mut self: Box<Self>) -> Result<(), String> {
        let context = self.execution_context.as_ref().ok_or_else(|| {
            "Builder deletion requires its admitted execution context".to_string()
        })?;
        self.store_execution_is_current(context).await?;
        if let Some(target) = &self.retirement_target {
            self.projects
                .check_captured_builder_registration(target, self.receipt.registry.as_ref())
                .await
                .map_err(|error| {
                    format!("Observe captured builder registration before deletion: {error:#}")
                })?;
        }
        self.operation.mutating =
            !self.snapshot.resources.is_empty() || self.snapshot.docker_bind_cleanup;
        for operation in &mut self.extra_leases {
            operation.begin_external_mutation()?;
        }
        self.runtime
            .delete_builder_snapshot(&self.snapshot)
            .await
            .map_err(|e| format!("delete captured builder: {e}"))?;
        // 协作者实例顺序删除；任一失败即保留未确认快照，交给原操作恢复。
        for (instance, snapshot) in &self.extra_instances {
            self.runtime
                .delete_builder_snapshot(snapshot)
                .await
                .map_err(|e| format!("delete collaborator builder ({instance}): {e}"))?;
        }
        if self.snapshot.docker_bind_cleanup {
            // snapshot.app_id 应用共享后恒纯 app_id——app_id 内部连字符已合法
            // （DNS-1123），不能按 '-' 右切还原（会把 ae2env-xxx 切成 xxx）
            for directory in &self.directories {
                shared_types::storage_contents::remove_captured_directory(
                    directory,
                    &self.snapshot.operation_id,
                )
                .await
                .map_err(|error| {
                    format!(
                        "Remove captured builder bind directory {}: {error}",
                        directory.path.display()
                    )
                })?;
            }
        }
        // Physical deletion has returned before logical SID ownership is
        // retired. A delayed old-lifecycle receipt never targets a replacement.
        self.store_execution_is_current(context).await?;
        self.projects
            .invalidate_userapp_agent_bindings_durable(
                &self.snapshot.app_id,
                &self.lifecycle_id,
                &format!(
                    "builder-agent-cleanup:{}:{}",
                    self.snapshot.operation_id, self.lifecycle_id
                ),
            )
            .await
            .map_err(|error| format!("Retire captured UserApp agent bindings: {error:#}"))?;
        self.store_execution_is_current(context).await?;
        if let Some(target) = &self.retirement_target {
            self.projects
                .retire_captured_builder_registration(target, self.receipt.registry.as_ref())
                .await
                .map_err(|error| format!("Retire captured builder registration: {error:#}"))?;
        } else {
            // An empty physical capture cannot authorize retirement of a later
            // physical instance. An absent/unbound registration is idempotent.
            let target = shared_types::BuilderControlTarget {
                context: context.clone(),
                resource_binding: None,
                workload: None,
                pod: None,
                restart_image: None,
                restart_runtime_workspace: None,
                builder_startup_mode: Default::default(),
            };
            self.projects
                .retire_captured_builder_registration(&target, self.receipt.registry.as_ref())
                .await
                .map_err(|error| format!("Retire absent builder registration: {error:#}"))?;
        }
        // 协作者实例注册表条目清理（key=复合 identifier；best-effort——注册是
        // 观测缓存，物理资源已删，残留条目经探活自愈路径自然失效）
        for (instance, _) in &self.extra_instances {
            // remove 返回 Option<Arc<..>>（带 Drop 语义）——显式绑定避免
            // let _ 立即析构告警；观测缓存条目删除本身即目的
            let _removed = self.projects.remove(instance);
        }
        crate::userapp_forward::invalidate_probe_cache(&self.snapshot.app_id);
        for (instance, _) in &self.extra_instances {
            crate::userapp_forward::invalidate_probe_cache(instance);
        }
        for mut operation in self.extra_leases.drain(..) {
            operation.finish_external_mutation().await?;
        }
        if let Some(operation) = self.operation.lease.take() {
            operation.release().await?;
        }
        tracing::info!(app_id = %self.snapshot.app_id, operation_id = %self.snapshot.operation_id, collaborators = self.extra_instances.len(), "Captured builder resources deleted");
        Ok(())
    }
}
async fn capture_bind_directories(app_id: &str) -> Result<Vec<CapturedStorageDirectory>, String> {
    // deploy-host：容器常量锚点在宿主机不存在（bind 源经 host_map 映射创建），
    // 清理须走同一映射出口，否则 read_dir NotFound 被吞 → dev bind 目录泄漏
    #[cfg(feature = "deploy-host")]
    let anchor =
        crate::utils::workspace_root_path(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT);
    #[cfg(not(feature = "deploy-host"))]
    let anchor = std::path::PathBuf::from(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT);
    let mut paths = Vec::new();
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
                    paths.push(user.path().join(suffix));
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("read builder bind root: {e}")),
    }
    paths.push(anchor.join(app_id));
    let mut directories = Vec::new();
    for path in paths {
        directories.push(
            StorageDirectoryLease::capture(&path)
                .await
                .map_err(|error| {
                    format!("Capture builder bind directory {}: {error}", path.display())
                })?
                .receipt,
        );
    }
    Ok(directories)
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
