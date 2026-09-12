//! Builder-only deletion by immutable Docker container ID.
use super::docker_runtime::DockerRuntime;
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use shared_types::{AppResourceIdentity, AppResourceKind, BuilderDeletionSnapshot, ServiceType};

impl DockerRuntime {
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
            lock_builder_file(&root, &name).map(|lease| UnclaimedBuilderLease(Some(lease)))
        })
        .await
        .map_err(|e| Error::DockerError(format!("builder operation lease worker: {e}")))??;
        let lease = pending
            .0
            .take()
            .ok_or_else(|| Error::DockerError("builder lease was already claimed".into()))?;
        Ok(Box::new(lease))
    }
    pub(super) async fn capture_builder(&self, app_id: &str) -> Result<BuilderDeletionSnapshot> {
        let mut snapshot = BuilderDeletionSnapshot {
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

fn builder_identity(
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

fn lock_builder_file(root: &std::path::Path, name: &str) -> Result<BuilderFileLease> {
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
    let marker = shared_types::AppFileMutationMarker::new();
    marker
        .begin(&file)
        .map_err(|e| Error::DockerError(format!("persist builder operation marker: {e}")))?;
    Ok(BuilderFileLease {
        file,
        marker,
        unlocked: false,
    })
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
