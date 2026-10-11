//! Business admission upgrades legacy proxy owners before capturing build
//! expectations. Stop and read-only owner observations never call this bridge.
use super::*;

impl DevServerManager {
    pub(in crate::service::dev_server) async fn ensure_official_owner_for_start(
        &self,
        project: &str,
        workspace: &Path,
        available: AvailableOwner,
        caller_deadline: tokio::time::Instant,
        preserve_build_expectation: bool,
    ) -> AppResult<AvailableOwner> {
        if supports_official(&available.identity) {
            return Ok(available);
        }
        if preserve_build_expectation
            && matches!(
                lock(&self.owner_expectations)?.get(project),
                Some(
                    super::super::types::OwnerExpectation::Captured { .. }
                        | super::super::types::OwnerExpectation::ObservationFailed { .. }
                )
            )
        {
            return Err(AppError::business(
                "owner protocol changed after build admission was captured; this request cannot upgrade the owner or refresh its Stop barrier",
            ));
        }
        let deadline = std::cmp::min(
            caller_deadline,
            tokio::time::Instant::now() + Duration::from_secs(45),
        );
        tokio::time::timeout_at(deadline, self.upgrade_official_owner(project, workspace, available, deadline))
            .await
            .map_err(|_| AppError::business("owner protocol upgrade timed out; management state is preserved and the original request may be retried"))?
            .map_err(|error| AppError::owner_error("upgrade official Pingap owner before business admission", error))
    }

    async fn upgrade_official_owner(
        &self,
        project: &str,
        workspace: &Path,
        available: AvailableOwner,
        deadline: tokio::time::Instant,
    ) -> Result<AvailableOwner> {
        let app = std::env::var("PROJECT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "unknown-app".into());
        owner_client::verify_project_identity(&available.identity, workspace, &app)?;
        let endpoint: std::net::SocketAddr = available
            .address
            .parse()
            .context("owner protocol upgrade requires numeric loopback")?;
        ensure!(
            endpoint.ip().is_loopback(),
            "owner protocol upgrade cannot bootstrap a remote application"
        );
        let (root, token) =
            owner_client::find_owner_token(Path::new(&available.identity.source_root), &app)
                .context("owner upgrade control credentials unavailable")?;
        let status = OwnerClient::new(&available.address, &token)?
            .status()
            .await?;
        ensure!(
            status.runtime_instance_id == available.identity.runtime_instance_id,
            "owner changed before protocol upgrade authentication"
        );
        let original_native = runtime_supervisor::control(
            &root,
            runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        )
        .await?;
        let origin = runtime_state_layout::resolve_project_origin(workspace)?;
        ensure!(
            original_native.binding.component == "app-cli"
                && runtime_state_layout::canonical_project_root(&original_native.binding.resource)
                    == runtime_state_layout::canonical_project_root(&origin),
            "owner protocol bootstrap authority belongs to another workspace"
        );
        // The new app-cli owns retirement: it authenticates the saved native
        // binding, captures Shutdown, checks domain/cleanup, and only then
        // acquires the original owner lock. The file-server never signals PID.
        let bootstrap = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("owner-bootstrap.lock"))?;
        let previous_child = lock(&self.owner_children)
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .get(project)
            .cloned();
        loop {
            match bootstrap.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    if let Some(successor) = self
                        .official_successor(project, workspace, &available, &root, status.revision)
                        .await?
                    {
                        return Ok(successor);
                    }
                    ensure!(
                        tokio::time::Instant::now() < deadline,
                        "owner protocol upgrade is still in progress"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => return Err(error).context("lock protocol owner bootstrap"),
            }
        }
        if let Some(successor) = self
            .official_successor(project, workspace, &available, &root, status.revision)
            .await?
        {
            return Ok(successor);
        }
        let current = owner_client::observe_owner(&available.address).await?;
        ensure!(
            matches!(current, OwnerProbe::Ready(ref identity)
            if same_identity(identity, &available.identity)),
            "owner changed before protocol upgrade bootstrap"
        );
        tracing::info!(project, instance = %available.identity.runtime_instance_id,
            "bootstrapping official proxy management before capturing build identity");
        self.spawn_owner_candidate(project, workspace, &available.address, true)
            .await?;
        loop {
            if let Some(successor) = self
                .official_successor(project, workspace, &available, &root, status.revision)
                .await?
            {
                return Ok(successor);
            }
            let exited = lock(&self.owner_children)
                .map_err(|error| anyhow::anyhow!("{error}"))?
                .get(project)
                .and_then(|child| child.exited());
            if let Some(exit) = exited {
                // A fixed supervisor can win the owner lease while our
                // control-only candidate finishes its dispatch. Its API may
                // still be initializing; a confirmed different native owner
                // is observed within the same deadline, never stopped again.
                if let Ok(native) = runtime_supervisor::control(
                    &root,
                    runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                )
                .await
                    && native.supervisor_id != original_native.supervisor_id
                    && native.binding == original_native.binding
                    && native.intent != runtime_supervisor::Intent::Shutdown
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                anyhow::bail!(
                    "configured app-cli bootstrap {} before an official owner was verified; install the matching runtime and retry",
                    exit.describe()
                );
            }
            // Retain the previous wait/reap observation through the handover;
            // replacing the map entry does not imply the old process exited.
            let _ = &previous_child;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn official_successor(
        &self,
        project: &str,
        workspace: &Path,
        original: &AvailableOwner,
        root: &Path,
        original_revision: u64,
    ) -> Result<Option<AvailableOwner>> {
        let OwnerProbe::Ready(identity) = owner_client::observe_owner(&original.address).await?
        else {
            return Ok(None);
        };
        if identity.runtime_instance_id == original.identity.runtime_instance_id {
            return Ok(None);
        }
        ensure!(
            supports_official(&identity),
            "replacement owner still lacks the official proxy restart contract"
        );
        owner_client::verify_project_identity(
            &identity,
            workspace,
            &original.identity.application_id,
        )?;
        let (found_root, token) = owner_client::find_owner_token(
            Path::new(&identity.source_root),
            &identity.application_id,
        )
        .context("replacement owner upgrade credentials unavailable")?;
        ensure!(
            found_root == root,
            "replacement owner changed the application state authority"
        );
        let status = OwnerClient::new(&original.address, &token)?
            .status()
            .await?;
        ensure!(
            status.runtime_instance_id == identity.runtime_instance_id
                && status.revision == original_revision,
            "control intent changed during owner protocol upgrade; a late build must not refresh its Stop barrier"
        );
        let native = runtime_supervisor::control(
            root,
            runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        )
        .await?;
        let origin = runtime_state_layout::resolve_project_origin(workspace)?;
        ensure!(
            native.binding.component == "app-cli"
                && runtime_state_layout::canonical_project_root(&native.binding.resource)
                    == runtime_state_layout::canonical_project_root(&origin),
            "replacement native owner belongs to another workspace"
        );
        if native.intent == runtime_supervisor::Intent::Shutdown {
            return Ok(None);
        }
        let ready = self
            .wait_for_recovery_owner(project, workspace, &original.address, root)
            .await?;
        ensure!(
            same_identity(&ready, &identity),
            "replacement owner changed while confirming native recovery"
        );
        Ok(Some(AvailableOwner {
            address: original.address.clone(),
            identity,
        }))
    }
}

fn supports_official(identity: &RuntimeIdentityView) -> bool {
    identity
        .capabilities
        .iter()
        .any(|capability| capability == shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY)
}

fn same_identity(left: &RuntimeIdentityView, right: &RuntimeIdentityView) -> bool {
    left.application_id == right.application_id
        && left.service_family == right.service_family
        && left.workspace_id == right.workspace_id
        && left.source_root == right.source_root
        && left.runtime_instance_id == right.runtime_instance_id
        && left.deployment_generation_id == right.deployment_generation_id
        && left.protocol_version == right.protocol_version
        && left.capabilities == right.capabilities
}

#[cfg(test)]
#[path = "official_tests.rs"]
mod tests;
