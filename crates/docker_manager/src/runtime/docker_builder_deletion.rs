//! Builder-only deletion by immutable Docker container ID.
use super::docker_runtime::DockerRuntime;
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use shared_types::{AppResourceIdentity, AppResourceKind, BuilderDeletionSnapshot, ServiceType};

impl DockerRuntime {
    pub(super) async fn captured_builder_workspace(
        &self,
        snapshot: &BuilderDeletionSnapshot,
        context: &shared_types::UserAppExecutionContext,
    ) -> Result<shared_types::UserAppBuilderWorkspaceEndpoint> {
        context
            .validate_identity(&snapshot.app_id)
            .map_err(Error::Conflict)?;
        let [resource] = snapshot.resources.as_slice() else {
            return Err(Error::Conflict(
                "Builder container receipt is ambiguous or missing".into(),
            ));
        };
        if resource.kind != AppResourceKind::Container || resource.uid.is_empty() {
            return Err(Error::Conflict(
                "Builder container receipt is invalid".into(),
            ));
        }
        let info = self
            .inner
            .get_docker_client()
            .inspect_container(&resource.uid, None)
            .await
            .map_err(|error| {
                Error::DockerError(format!("Inspect captured builder endpoint: {error}"))
            })?;
        workspace_endpoint_from_bound_container(
            &info,
            resource,
            context,
            snapshot.resource_binding.as_ref(),
        )
    }

    pub(super) async fn acquire_builder_lease(
        &self,
        app_id: &str,
    ) -> Result<Box<dyn shared_types::AppOperationLease>> {
        self.acquire_application_file_lease(app_id, &ServiceType::UserappBuilder)
            .await
    }
    pub(super) async fn acquire_application_file_lease(
        &self,
        app_id: &str,
        family: &ServiceType,
    ) -> Result<Box<dyn shared_types::AppOperationLease>> {
        self.acquire_application_file_lease_with_context(app_id, family, None)
            .await
    }

    pub(super) async fn acquire_application_file_lease_with_context(
        &self,
        app_id: &str,
        family: &ServiceType,
        context: Option<&shared_types::UserAppExecutionContext>,
    ) -> Result<Box<dyn shared_types::AppOperationLease>> {
        let marker = if let Some(context) = context {
            context
                .validate_identity(app_id)
                .map_err(Error::ConfigurationError)?;
            shared_types::AppFileMutationMarker::for_operation(&context.operation_id)
                .map_err(|error| Error::ConfigurationError(error.to_string()))?
        } else {
            shared_types::AppFileMutationMarker::new()
        };
        let prefix = match family {
            ServiceType::Userapp => "prod",
            ServiceType::UserappBuilder => "builder",
            _ => {
                return Err(Error::ConfigurationError(
                    "application lease requires UserApp family".into(),
                ));
            }
        };
        if app_id.is_empty()
            || app_id.len() > 64
            || !app_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(Error::ConfigurationError(
                "invalid builder app identifier".into(),
            ));
        }
        let root = std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT)
            .join(".app-operation-locks");
        let name = format!("{prefix}-{app_id}.lock");
        let mut pending = tokio::task::spawn_blocking(move || {
            lock_builder_file_with_marker(&root, &name, marker)
                .map(|lease| UnclaimedBuilderLease(Some(lease)))
        })
        .await
        .map_err(|e| Error::DockerError(format!("builder operation lease worker: {e}")))??;
        let mut lease = pending
            .0
            .take()
            .ok_or_else(|| Error::DockerError("builder lease was already claimed".into()))?;
        lease.service_type = family.clone();
        Ok(Box::new(lease))
    }
    pub(super) async fn release_captured_file_lease(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> Result<()> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::ConfigurationError)?;
        receipt.validate().map_err(Error::ConfigurationError)?;
        let prefix = match receipt.service_type() {
            ServiceType::Userapp => "prod",
            ServiceType::UserappBuilder => "builder",
            _ => {
                return Err(Error::ConfigurationError(
                    "Invalid application lease family".into(),
                ));
            }
        };
        let path = std::path::Path::new(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT)
            .join(".app-operation-locks")
            .join(format!("{prefix}-{}.lock", context.app_id));
        let receipt = receipt.clone();
        tokio::task::spawn_blocking(move || release_file_receipt(&path, &receipt))
            .await
            .map_err(|error| {
                Error::DockerError(format!("Operation lease cleanup worker failed: {error}"))
            })?
    }

    pub(super) async fn capture_builder(&self, app_id: &str) -> Result<BuilderDeletionSnapshot> {
        let mut snapshot = BuilderDeletionSnapshot {
            resource_binding: None,
            app_id: app_id.into(),
            operation_id: uuid::Uuid::new_v4().to_string(),
            resources: vec![],
            docker_bind_cleanup: true,
        };
        // Read Docker directly: runtime find synthesizes service_type from the
        // request and may return cached IDs, neither proves physical ownership.
        let name = crate::utils::DockerUtils::generate_container_name(
            ServiceType::UserappBuilder.container_prefix(),
            app_id,
        )
        .map_err(Error::ConfigurationError)?;
        match self
            .inner
            .get_docker_client()
            .inspect_container(&name, None)
            .await
        {
            Ok(info) => snapshot
                .resources
                .push(builder_identity(info, &name, app_id)?),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {}
            Err(error) => {
                return Err(Error::DockerError(format!(
                    "capture builder container: {error}"
                )));
            }
        }
        Ok(snapshot)
    }

    pub(super) async fn delete_captured_builder(
        &self,
        snapshot: &BuilderDeletionSnapshot,
    ) -> Result<()> {
        if !snapshot.docker_bind_cleanup {
            return Err(Error::ConfigurationError(
                "non-Docker builder receipt".into(),
            ));
        }
        let current = self.capture_builder(&snapshot.app_id).await?;
        if current
            .resources
            .iter()
            .any(|resource| !snapshot.resources.contains(resource))
        {
            return Err(Error::Conflict(
                "builder was replaced after deletion capture".into(),
            ));
        }
        for resource in &snapshot.resources {
            if resource.kind != AppResourceKind::Container || resource.uid.is_empty() {
                return Err(Error::ConfigurationError(
                    "invalid builder container receipt".into(),
                ));
            }
            match self
                .inner
                .get_docker_client()
                .remove_container(
                    &resource.uid,
                    Some(bollard::query_parameters::RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                Ok(()) => {}
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {}
                Err(error) => {
                    return Err(Error::DockerError(format!(
                        "delete captured builder: {error}"
                    )));
                }
            }
            self.inner
                .retire_container_cache(&resource.uid)
                .await
                .map_err(|error| {
                    Error::DockerError(format!("retire deleted container cache: {error}"))
                })?;
        }
        if !self
            .capture_builder(&snapshot.app_id)
            .await?
            .resources
            .is_empty()
        {
            return Err(Error::Conflict(
                "builder was replaced during deletion".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
fn workspace_endpoint_from_container(
    info: &bollard::models::ContainerInspectResponse,
    resource: &AppResourceIdentity,
    context: &shared_types::UserAppExecutionContext,
) -> Result<shared_types::UserAppBuilderWorkspaceEndpoint> {
    workspace_endpoint_from_bound_container(info, resource, context, None)
}

fn workspace_endpoint_from_bound_container(
    info: &bollard::models::ContainerInspectResponse,
    resource: &AppResourceIdentity,
    context: &shared_types::UserAppExecutionContext,
    binding: Option<&shared_types::UserAppResourceBinding>,
) -> Result<shared_types::UserAppBuilderWorkspaceEndpoint> {
    let actual = super::docker_builder_control::control_identity_with_binding(
        info,
        &resource.name,
        context,
        binding,
        false,
    )?;
    if actual != *resource {
        return Err(Error::Conflict(
            "Captured builder container identity changed".into(),
        ));
    }
    if info.state.as_ref().and_then(|state| state.running) != Some(true) {
        return Err(Error::Conflict(
            "Captured builder container is not running".into(),
        ));
    }
    let preferred = info
        .host_config
        .as_ref()
        .and_then(|config| config.network_mode.as_deref());
    let address = super::docker_runtime::extract_container_ip(info, preferred)
        .parse::<std::net::IpAddr>()
        .map_err(|error| {
            Error::ConfigurationError(format!("Invalid builder endpoint address: {error}"))
        })?;
    if address.is_unspecified() {
        return Err(Error::ConfigurationError(
            "Builder endpoint address is unspecified".into(),
        ));
    }
    Ok(shared_types::UserAppBuilderWorkspaceEndpoint {
        container_id: actual.uid,
        address,
    })
}

pub(super) fn builder_identity(
    info: bollard::models::ContainerInspectResponse,
    name: &str,
    app_id: &str,
) -> Result<AppResourceIdentity> {
    let labels = info
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or_else(|| Error::Conflict("builder container has no ownership labels".into()))?;
    if labels.get("service-type").map(String::as_str)
        != Some(ServiceType::UserappBuilder.to_string().as_str())
        || labels.get("identifier").map(String::as_str) != Some(app_id)
    {
        return Err(Error::Conflict(
            "builder container ownership mismatch".into(),
        ));
    }
    let uid = info
        .id
        .filter(|id| !id.is_empty())
        .ok_or_else(|| Error::DockerError("builder container has no physical ID".into()))?;
    Ok(AppResourceIdentity {
        kind: AppResourceKind::Container,
        name: name.to_owned(),
        uid,
        resource_version: None,
    })
}

#[cfg(test)]
fn lock_builder_file(root: &std::path::Path, name: &str) -> Result<BuilderFileLease> {
    lock_builder_file_with_marker(root, name, shared_types::AppFileMutationMarker::new())
}

fn lock_builder_file_with_marker(
    root: &std::path::Path,
    name: &str,
    marker: shared_types::AppFileMutationMarker,
) -> Result<BuilderFileLease> {
    std::fs::create_dir_all(root)
        .map_err(|e| Error::DockerError(format!("builder operation lock directory: {e}")))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(name))
        .map_err(|e| Error::DockerError(format!("builder operation lock file: {e}")))?;
    file.try_lock()
        .map_err(|e| Error::Conflict(format!("builder operation lease unavailable: {e}")))?;
    shared_types::AppFileMutationMarker::check_clean(&file)
        .map_err(|e| Error::Conflict(format!("builder operation requires recovery: {e}")))?;
    marker
        .begin(&file)
        .map_err(|e| Error::DockerError(format!("persist builder operation marker: {e}")))?;
    Ok(BuilderFileLease {
        file,
        marker,
        unlocked: false,
        service_type: ServiceType::UserappBuilder,
    })
}

#[cfg(unix)]
fn release_file_receipt(
    path: &std::path::Path,
    receipt: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let shared_types::UserAppOperationLeaseReceipt::Docker {
        device,
        inode,
        token,
        ..
    } = receipt
    else {
        return Err(Error::Conflict("Operation lease runtime mismatch".into()));
    };
    let before = std::fs::symlink_metadata(path)
        .map_err(|error| Error::Conflict(format!("Operation lease file unavailable: {error}")))?;
    if !before.is_file() || before.dev() != *device || before.ino() != *inode {
        return Err(Error::Conflict(
            "Operation lease file identity changed".into(),
        ));
    }
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| Error::DockerError(format!("Open operation lease receipt: {error}")))?;
    file.try_lock()
        .map_err(|error| Error::Conflict(format!("Operation lease remains active: {error}")))?;
    let opened = file
        .metadata()
        .map_err(|error| Error::DockerError(format!("Read operation lease identity: {error}")))?;
    let current = std::fs::symlink_metadata(path)
        .map_err(|error| Error::Conflict(format!("Recheck operation lease identity: {error}")))?;
    if opened.dev() != *device
        || opened.ino() != *inode
        || !current.is_file()
        || current.dev() != *device
        || current.ino() != *inode
    {
        return Err(Error::Conflict(
            "Operation lease file identity changed".into(),
        ));
    }
    let marker = shared_types::AppFileMutationMarker::for_operation(token)
        .map_err(|error| Error::ConfigurationError(error.to_string()))?;
    marker
        .complete(&file)
        .map_err(|error| Error::Conflict(format!("Operation lease ownership changed: {error}")))?;
    file.unlock()
        .map_err(|error| Error::DockerError(format!("Unlock completed operation lease: {error}")))
}

#[cfg(not(unix))]
fn release_file_receipt(
    _: &std::path::Path,
    _: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<()> {
    Err(Error::ConfigurationError(
        "Physical operation lease recovery requires Unix".into(),
    ))
}

// Cancellation before the blocking lock-acquisition result is delivered cannot
// have admitted a resource mutation. Reclaim that marker instead of stranding it.
struct UnclaimedBuilderLease(Option<BuilderFileLease>);
impl Drop for UnclaimedBuilderLease {
    fn drop(&mut self) {
        if let Some(lease) = self.0.take()
            && let Err(error) = lease.marker.complete(&lease.file)
        {
            tracing::error!(%error, "Failed to release unclaimed builder lease");
        }
    }
}

struct BuilderFileLease {
    service_type: ServiceType,
    file: std::fs::File,
    marker: shared_types::AppFileMutationMarker,
    unlocked: bool,
}
impl Drop for BuilderFileLease {
    fn drop(&mut self) {
        if !self.unlocked
            && let Err(error) = self.file.unlock()
        {
            tracing::error!(%error, "unlock builder operation file failed");
        }
    }
}
#[async_trait::async_trait]
impl shared_types::AppOperationLease for BuilderFileLease {
    fn receipt(&self) -> Option<shared_types::UserAppOperationLeaseReceipt> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = match self.file.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::error!(%error, "Read builder lease identity failed");
                    return None;
                }
            };
            Some(shared_types::UserAppOperationLeaseReceipt::Docker {
                service_type: self.service_type.clone(),
                device: metadata.dev(),
                inode: metadata.ino(),
                token: self.marker.operation_id().to_owned(),
            })
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    async fn release(mut self: Box<Self>) -> std::result::Result<(), String> {
        self.marker
            .complete(&self.file)
            .map_err(|error| format!("complete builder operation marker: {error}"))?;
        self.file
            .unlock()
            .map_err(|error| format!("release builder operation lease: {error}"))?;
        self.unlocked = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::AppOperationLease;

    #[cfg(unix)]
    #[test]
    fn captured_file_release_requires_inactive_original_inode_and_owner() {
        let root = tempfile::tempdir().expect("fixture");
        let path = root.path().join("builder-app.lock");
        let lease = lock_builder_file_with_marker(
            root.path(),
            "builder-app.lock",
            shared_types::AppFileMutationMarker::for_operation("original").expect("marker"),
        )
        .expect("lease");
        let receipt = lease.receipt().expect("receipt");
        assert!(
            release_file_receipt(&path, &receipt).is_err(),
            "live lease cannot be reclaimed"
        );
        assert_eq!(std::fs::read_to_string(&path).expect("marker"), "original");
        drop(lease);
        release_file_receipt(&path, &receipt).expect("completed orphan release");
        release_file_receipt(&path, &receipt).expect("idempotent same inode release");
        assert!(std::fs::read(&path).expect("empty marker").is_empty());
        let next = lock_builder_file_with_marker(
            root.path(),
            "builder-app.lock",
            shared_types::AppFileMutationMarker::for_operation("replacement-owner")
                .expect("marker"),
        )
        .expect("next owner");
        drop(next);
        assert!(release_file_receipt(&path, &receipt).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("replacement marker"),
            "replacement-owner"
        );
        std::fs::rename(&path, root.path().join("retained-original-inode"))
            .expect("keep inode alive");
        std::fs::write(&path, "new-physical-file").expect("replacement");
        assert!(release_file_receipt(&path, &receipt).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("replacement file"),
            "new-physical-file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn captured_file_release_rejects_symbolic_link_even_to_original_inode() {
        let root = tempfile::tempdir().expect("fixture");
        let path = root.path().join("builder-app.lock");
        let retained = root.path().join("retained");
        let lease = lock_builder_file_with_marker(
            root.path(),
            "builder-app.lock",
            shared_types::AppFileMutationMarker::for_operation("original").expect("marker"),
        )
        .expect("lease");
        let receipt = lease.receipt().expect("receipt");
        drop(lease);
        std::fs::rename(&path, &retained).expect("rename");
        std::os::unix::fs::symlink(&retained, &path).expect("alias");
        assert!(release_file_receipt(&path, &receipt).is_err());
        assert_eq!(
            std::fs::read_to_string(&retained).expect("original marker"),
            "original"
        );
    }

    #[tokio::test]
    async fn durable_operation_marker_echoes_identity_without_authorizing_takeover() {
        let root = tempfile::tempdir().unwrap();
        let name = "builder-durable.lock";
        let marker = shared_types::AppFileMutationMarker::for_operation("operation-one").unwrap();
        let lease = lock_builder_file_with_marker(root.path(), name, marker).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let physical = std::fs::metadata(root.path().join(name)).unwrap();
            let receipt = lease.receipt().expect("durable lease receipt");
            assert_eq!(
                receipt,
                shared_types::UserAppOperationLeaseReceipt::Docker {
                    service_type: ServiceType::UserappBuilder,
                    device: physical.dev(),
                    inode: physical.ino(),
                    token: "operation-one".into(),
                }
            );
            receipt.validate().expect("valid physical receipt");
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join(name)).unwrap(),
            "operation-one"
        );
        Box::new(lease).release().await.unwrap();
        assert!(std::fs::read(root.path().join(name)).unwrap().is_empty());

        let marker = shared_types::AppFileMutationMarker::for_operation("operation-one").unwrap();
        drop(lock_builder_file_with_marker(root.path(), name, marker).unwrap());
        // Even an identical durable operation must prove recovery safety first.
        let marker = shared_types::AppFileMutationMarker::for_operation("operation-one").unwrap();
        assert!(lock_builder_file_with_marker(root.path(), name, marker).is_err());
        assert_eq!(
            std::fs::read_to_string(root.path().join(name)).unwrap(),
            "operation-one"
        );
    }

    #[tokio::test]
    async fn failed_builder_completion_clears_only_confirmed_rejection_marker() {
        for status in [403, 409, 408, 500] {
            let root = tempfile::tempdir().unwrap();
            let lease = lock_builder_file(root.path(), "builder-test.lock").unwrap();
            let failure =
                super::super::builder_completion::docker_error(crate::DockerError::BollardError(
                    bollard::errors::Error::DockerResponseServerError {
                        status_code: status,
                        message: "failed".into(),
                    },
                ));
            let result: container_runtime_api::ContainerRuntimeResult<()> =
                super::super::builder_completion::finish(Box::new(lease), Err(failure)).await;
            assert!(result.is_err());
            let next = lock_builder_file(root.path(), "builder-test.lock");
            if matches!(status, 403 | 409) {
                Box::new(next.expect("confirmed rejection permits retry"))
                    .release()
                    .await
                    .unwrap();
            } else {
                assert!(next.is_err(), "unconfirmed outcome must require recovery");
            }
        }
    }

    #[tokio::test]
    async fn failed_marker_release_does_not_erase_other_owner_or_original_error() {
        let root = tempfile::tempdir().unwrap();
        let lease = lock_builder_file(root.path(), "builder-test.lock").unwrap();
        std::fs::write(root.path().join("builder-test.lock"), "replacement-owner").unwrap();
        let failure = container_runtime_api::ContainerRuntimeError::RequestRejected(
            shared_types::RuntimeRequestRejection::from_status(403, "original denial".into())
                .unwrap(),
        );
        let result: container_runtime_api::ContainerRuntimeResult<()> =
            super::super::builder_completion::finish(Box::new(lease), Err(failure)).await;
        assert!(result.unwrap_err().to_string().contains("original denial"));
        assert_eq!(
            std::fs::read_to_string(root.path().join("builder-test.lock")).unwrap(),
            "replacement-owner"
        );
        assert!(lock_builder_file(root.path(), "builder-test.lock").is_err());
    }
    #[tokio::test]
    async fn captured_delete_retires_only_deleted_identity_caches() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let name = "rcoder-app-builder-one";
        let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
        tokio::spawn(actor.run());
        let manager = Arc::new(crate::DockerManager {
            docker: bollard::Docker::connect_with_http(
                &format!("http://{address}"),
                1,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            config: crate::DockerManagerConfig::default(),
            containers,
            main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
            api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
        });
        let query = |id: &str| {
            Arc::new(crate::ContainerQueryResult::new(
                id.into(),
                name.into(),
                crate::ContainerStatus::Running,
                true,
                "192.0.2.1".into(),
                chrono::Utc::now(),
            ))
        };
        manager
            .api_cache
            .insert_status("old-id".into(), Some(query("old-id")))
            .await;
        manager
            .api_cache
            .insert_status(name.into(), Some(query("old-id")))
            .await;
        manager
            .api_cache
            .insert_network("old-id".into(), Some(Arc::new(Default::default())))
            .await;
        manager
            .containers
            .insert(
                "old-alias".into(),
                crate::DockerContainerInfo::new(
                    "old-id".into(),
                    name.into(),
                    "one".into(),
                    "image".into(),
                ),
            )
            .await;
        manager
            .containers
            .insert(
                "new-alias".into(),
                crate::DockerContainerInfo::new(
                    "new-id".into(),
                    name.into(),
                    "one".into(),
                    "image".into(),
                ),
            )
            .await;
        let writer = manager.clone();
        let replacement = query("new-id");
        let server = tokio::spawn(async move {
            for phase in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 8192];
                let count = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..count]);
                let (status,body) = match phase {
                    0 => (200, serde_json::json!({"Id":"old-id","Name":format!("/{name}"),"Config":{"Labels":{"service-type":ServiceType::UserappBuilder.to_string(),"identifier":"one"}}}).to_string()),
                    1 => {
                        assert!(request.starts_with("DELETE ") && request.contains("/containers/old-id?"),"{request}");
                        writer.api_cache.insert_status(name.into(),Some(replacement.clone())).await;
                        (204,String::new())
                    },
                    _ => (404,r#"{"message":"not found"}"#.into()),
                };
                let response = format!(
                    "HTTP/1.1 {status} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let runtime = DockerRuntime::new(manager.clone());
        let snapshot = BuilderDeletionSnapshot {
            resource_binding: None,
            app_id: "one".into(),
            operation_id: "test".into(),
            docker_bind_cleanup: true,
            resources: vec![AppResourceIdentity {
                kind: AppResourceKind::Container,
                name: name.into(),
                uid: "old-id".into(),
                resource_version: None,
            }],
        };
        runtime.delete_captured_builder(&snapshot).await.unwrap();
        server.await.unwrap();
        assert!(manager.containers.get("old-alias").await.is_none());
        assert_eq!(
            manager
                .containers
                .get("new-alias")
                .await
                .unwrap()
                .container_id,
            "new-id"
        );
        assert!(manager.api_cache.get_status("old-id").await.is_none());
        assert!(manager.api_cache.get_network("old-id").await.is_none());
        assert_eq!(
            manager
                .api_cache
                .get_status(name)
                .await
                .unwrap()
                .unwrap()
                .container_id,
            "new-id"
        );
    }

    #[test]
    fn captured_builder_requires_actual_family_and_identifier_labels() {
        let inspect = |family: &str, identifier: &str| {
            serde_json::from_value(serde_json::json!({
                "Id": "immutable-docker-id", "Name": "/rcoder-app-builder-one",
                "Config": { "Labels": {"service-type": family, "identifier": identifier} }
            }))
            .unwrap()
        };
        let name = "rcoder-app-builder-one";
        let family = ServiceType::UserappBuilder.to_string();
        let valid = builder_identity(inspect(&family, "one"), name, "one").unwrap();
        assert_eq!(valid.uid, "immutable-docker-id");
        assert_eq!(valid.name, name);
        for foreign in [
            ServiceType::Userapp,
            ServiceType::WebAgentRunner,
            ServiceType::ComputerAgentRunner,
        ] {
            assert!(matches!(
                builder_identity(inspect(&foreign.to_string(), "one"), name, "one"),
                Err(Error::Conflict(_))
            ));
        }
        assert!(matches!(
            builder_identity(inspect(&family, "other"), name, "one"),
            Err(Error::Conflict(_))
        ));
        assert!(matches!(
            builder_identity(
                bollard::models::ContainerInspectResponse::default(),
                name,
                "one"
            ),
            Err(Error::Conflict(_))
        ));
        let mut missing_id = inspect(&family, "one");
        missing_id.id = Some(String::new());
        assert!(matches!(
            builder_identity(missing_id, name, "one"),
            Err(Error::DockerError(_))
        ));
    }

    #[tokio::test]
    async fn completed_builder_file_lease_allows_recreation() {
        let root = tempfile::tempdir().unwrap();
        let captured = lock_builder_file(root.path(), "builder-one.lock").unwrap();
        assert!(matches!(
            lock_builder_file(root.path(), "builder-one.lock"),
            Err(Error::Conflict(_))
        ));
        let unrelated = lock_builder_file(root.path(), "builder-two.lock").unwrap();
        Box::new(unrelated).release().await.unwrap();
        Box::new(captured).release().await.unwrap();
        let next = lock_builder_file(root.path(), "builder-one.lock").unwrap();
        Box::new(next).release().await.unwrap();
    }
    #[test]
    fn undelivered_lock_acquisition_does_not_strand_marker() {
        let root = tempfile::tempdir().unwrap();
        drop(UnclaimedBuilderLease(Some(
            lock_builder_file(root.path(), "builder-one.lock").unwrap(),
        )));
        let reclaimed = lock_builder_file(root.path(), "builder-one.lock").unwrap();
        drop(UnclaimedBuilderLease(Some(reclaimed)));
    }
    #[test]
    fn uncertain_builder_mutation_retains_marker_after_file_unlock() {
        let root = tempfile::tempdir().unwrap();
        drop(lock_builder_file(root.path(), "builder-one.lock").unwrap());
        assert!(matches!(
            lock_builder_file(root.path(), "builder-one.lock"),
            Err(Error::Conflict(_))
        ));
    }
    #[tokio::test]
    async fn builder_drop_unlocks_live_duplicate_and_preserves_only_uncertain_marker() {
        for completed in [false, true] {
            let root = tempfile::tempdir().expect("directory");
            let lease = lock_builder_file(root.path(), "builder-duplicate.lock").expect("lease");
            let duplicate = lease.file.try_clone().expect("duplicate descriptor");
            if completed {
                Box::new(lease).release().await.expect("complete");
            } else {
                drop(lease);
            }
            let next = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(root.path().join("builder-duplicate.lock"))
                .expect("next file");
            next.try_lock()
                .expect("Drop explicitly unlocked despite live duplicate");
            assert_eq!(next.metadata().expect("metadata").len() > 0, !completed);
            next.unlock().expect("next unlock");
            drop(duplicate);
        }
    }
}

#[cfg(test)]
mod workspace_endpoint_tests {
    use super::*;

    #[test]
    fn endpoint_requires_captured_id_lifecycle_family_and_running_address() {
        let context = shared_types::UserAppExecutionContext {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            operation_id: "clear".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let resource = AppResourceIdentity {
            kind: AppResourceKind::Container,
            name: "builder-app".into(),
            uid: "physical-original".into(),
            resource_version: None,
        };
        let mut labels = context.resource_metadata();
        labels.insert(
            "service-type".into(),
            ServiceType::UserappBuilder.to_string(),
        );
        labels.insert("identifier".into(), "app".into());
        let original = serde_json::json!({
            "Id": resource.uid, "Config": {"Labels": labels}, "State": {"Running": true},
            "HostConfig": {"NetworkMode": "main"},
            "NetworkSettings": {"Networks": {"main": {"IPAddress": "172.20.0.4"}}}
        });
        let inspect = serde_json::from_value(original.clone()).expect("inspect fixture");
        let endpoint = workspace_endpoint_from_container(&inspect, &resource, &context)
            .expect("physical target");
        assert_eq!(endpoint.container_id, "physical-original");
        assert_eq!(
            endpoint.base_url(),
            format!("http://172.20.0.4:{}", shared_types::AGENT_FILE_SERVER_PORT)
        );
        for (pointer, value) in [
            ("/Id", serde_json::json!("replacement")),
            (
                "/Config/Labels/rcoder.io~1lifecycle-id",
                serde_json::json!("next-life"),
            ),
            (
                "/Config/Labels/service-type",
                serde_json::json!(ServiceType::Userapp.to_string()),
            ),
            (
                "/Config/Labels/identifier",
                serde_json::json!("another-app"),
            ),
            ("/State/Running", serde_json::json!(false)),
            (
                "/NetworkSettings/Networks/main/IPAddress",
                serde_json::json!(""),
            ),
        ] {
            let mut changed = original.clone();
            *changed.pointer_mut(pointer).expect("fixture field") = value;
            let inspect = serde_json::from_value(changed).expect("changed fixture");
            assert!(
                workspace_endpoint_from_container(&inspect, &resource, &context).is_err(),
                "accepted changed field {pointer}"
            );
        }
    }
}
