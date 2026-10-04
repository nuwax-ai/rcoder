//! Retire an identity-verified, same-application owner bound to a stale child root.
//! Shutdown releases management ownership; StopWork would keep the wrong binding.
use super::*;
use runtime_state_layout::ManagedWorkspace;
use runtime_supervisor::{Action, Request, Snapshot};

impl DevServerManager {
    pub(super) async fn accept_or_repair_owner(
        &self,
        project: &str,
        workspace: &Path,
        address: &str,
        identity: RuntimeIdentityView,
    ) -> Result<RuntimeIdentityView> {
        // Start and Stop have different standalone identity rules: Stop may
        // retire a contained manual deploy directory with no origin marker.
        // Preserve those existing callers' checks outside the managed builder.
        if !ManagedWorkspace::enabled_from_values(|key| std::env::var_os(key)) {
            return Ok(identity);
        }
        let root = runtime_state_layout::resolve_state_root(
            workspace,
            std::env::var_os("APP_CLI_STATE_ROOT").as_deref(),
            std::env::var_os("PROJECT_ID").as_deref(),
        )?
        .context("managed owner recovery has no declared state authority")?;
        let managed = ManagedWorkspace::from_env(workspace, &root)?
            .context("managed builder declaration changed while inspecting owner identity")?;
        if owner_client::verify_project_identity(&identity, workspace, &managed.application_id)
            .is_ok()
        {
            return self
                .wait_for_recovery_owner_with_context(
                    project,
                    workspace,
                    address,
                    &root,
                    Some(&managed),
                )
                .await;
        }
        self.handover_managed_owner(project, address, identity, &managed)
            .await
    }

    async fn handover_managed_owner(
        &self,
        project: &str,
        address: &str,
        identity: RuntimeIdentityView,
        managed: &ManagedWorkspace,
    ) -> Result<RuntimeIdentityView> {
        let endpoint: std::net::SocketAddr = address
            .parse()
            .context("managed owner handover requires a numeric loopback endpoint")?;
        ensure!(
            endpoint.ip().is_loopback(),
            "managed owner handover requires a local endpoint"
        );
        verify_managed_identity(&identity, managed)?;
        let root = &managed.state_root;
        // One platform caller owns this transition. Contending callers observe
        // the successor, without issuing additional shutdown identities.
        let bootstrap = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("owner-bootstrap.lock"))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
        tokio::time::timeout_at(deadline, async {
        loop {
            match bootstrap.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    if let Some(successor) = verified_successor(address, managed, &identity.runtime_instance_id).await? {
                        return Ok(successor);
                    }
                    ensure!(
                        tokio::time::Instant::now() < deadline,
                        "managed owner handover is still in progress; retry this request"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => return Err(error).context("lock managed owner handover"),
            }
        }
            // Capture again under the bootstrap lock. Never act on an old HTTP
            // observation after another caller has already replaced its owner.
            let current = match owner_client::observe_owner(address).await? {
                OwnerProbe::Ready(current) => current,
                _ => anyhow::bail!("managed owner changed before handover; retry this request"),
            };
            if owner_client::verify_project_identity(&current, &managed.source_root, &managed.application_id).is_ok() {
                return verified_successor(address, managed, &identity.runtime_instance_id).await?
                    .context("replacement changed during managed handover verification");
            }
            ensure!(current.runtime_instance_id == identity.runtime_instance_id, "managed owner changed before handover; retry this request");
            verify_managed_identity(&current, managed)?;
            let before = runtime_supervisor::control(root, Request::new(Action::Status)).await?;
            verify_supervisor_binding(&before, &current, managed)?;
            // identity.json belongs to the same state authority as supervisor.json.
            // Check again after native Status so neither an unrelated HTTP server
            // nor a concurrent owner replacement can supply the target identity.
            verify_state_identity(&current, root)?;
            let mut request = Request::new(Action::Shutdown);
            request.capture_generation(before.generation.as_deref());
            if let Err(error) = runtime_supervisor::shutdown_captured_owner(root, &before, &request, deadline.saturating_duration_since(tokio::time::Instant::now())).await {
                if let Some(successor) = verified_successor(address, managed, &current.runtime_instance_id).await? {
                    return Ok(successor);
                }
                return Err(error);
            }
            tracing::info!(project, old_instance = %current.runtime_instance_id,
                old_root = %current.source_root, source_root = %managed.source_root.display(),
                "managed owner cleanup and ownership release confirmed; restoring source-root management");
            // A tracked process may release its owner lock just before its wait
            // task publishes ExitStatus. Do not mistake that window for a live
            // candidate and skip launching the correct-root management process.
            loop {
                let running = lock(&self.owner_children).map_err(|error| anyhow::anyhow!("{error}"))?
                    .get(project).is_some_and(|child| child.exited().is_none());
                if !running { break; }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if let Some(successor) = verified_successor(address, managed, &current.runtime_instance_id).await? {
                return Ok(successor);
            }
            self.spawn_recovery_owner(project, &managed.source_root, address).await?;
            let successor = self.wait_for_recovery_owner_with_context(project, &managed.source_root, address, root, Some(managed)).await?;
            owner_client::verify_project_identity(&successor, &managed.source_root, &managed.application_id)?;
            ensure!(successor.runtime_instance_id != current.runtime_instance_id,
                "managed owner handover returned the retired runtime identity");
            verify_state_identity(&successor, root)?;
            Ok(successor)
        }).await.context("managed owner handover timed out; original state retained and a later request may retry")?
    }
}

async fn verified_successor(
    address: &str,
    managed: &ManagedWorkspace,
    previous: &str,
) -> Result<Option<RuntimeIdentityView>> {
    loop {
        let OwnerProbe::Ready(identity) = owner_client::observe_owner(address).await? else {
            return Ok(None);
        };
        if identity.runtime_instance_id == previous
            || owner_client::verify_project_identity(
                &identity,
                &managed.source_root,
                &managed.application_id,
            )
            .is_err()
        {
            return Ok(None);
        }
        verify_state_identity(&identity, &managed.state_root)?;
        let native =
            runtime_supervisor::control(&managed.state_root, Request::new(Action::Status)).await?;
        ensure!(
            native.binding.component == "app-cli",
            "successor state belongs to another component"
        );
        managed.verify_contained_workspace(&native.binding.resource)?;
        if native.binding.resource != managed.source_root
            || !matches!(
                native.phase,
                runtime_supervisor::Phase::Ready | runtime_supervisor::Phase::Stopped
            )
            || native.intent == runtime_supervisor::Intent::Shutdown
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        verify_supervisor_binding(&native, &identity, managed)?;
        let token = owner_client::read_owner_token(&managed.state_root)
            .context("successor credentials unavailable")?;
        let recovery = OwnerClient::new(address, &token)?.recovery().await?;
        ensure!(
            recovery.runtime_instance_id == identity.runtime_instance_id,
            "successor changed during managed recovery verification"
        );
        return Ok(Some(identity));
    }
}

fn verify_managed_identity(
    identity: &RuntimeIdentityView,
    managed: &ManagedWorkspace,
) -> Result<()> {
    ensure!(
        owner_client::protocol_compatible(identity)
            && identity.application_id == managed.application_id
            && identity.service_family == "userapp-dev"
            && !identity.runtime_instance_id.trim().is_empty()
            && !identity.workspace_id.trim().is_empty(),
        "owner is not a compatible runtime for this managed application"
    );
    managed.verify_contained_workspace(Path::new(&identity.source_root))?;
    verify_state_identity(identity, &managed.state_root)
}

fn verify_state_identity(identity: &RuntimeIdentityView, root: &Path) -> Result<()> {
    let saved: RuntimeIdentityView = serde_json::from_slice(
        &std::fs::read(root.join("identity.json")).context("read managed owner state identity")?,
    )
    .context("decode managed owner state identity")?;
    ensure!(
        saved.runtime_instance_id == identity.runtime_instance_id
            && saved.application_id == identity.application_id
            && saved.service_family == identity.service_family
            && saved.source_root == identity.source_root
            && saved.workspace_id == identity.workspace_id
            && saved.protocol_version == identity.protocol_version,
        "HTTP owner does not belong to the managed state authority"
    );
    Ok(())
}

fn verify_supervisor_binding(
    snapshot: &Snapshot,
    identity: &RuntimeIdentityView,
    managed: &ManagedWorkspace,
) -> Result<()> {
    ensure!(
        snapshot.binding.component == "app-cli",
        "managed state authority belongs to another component"
    );
    managed.verify_contained_workspace(&snapshot.binding.resource)?;
    let origin = runtime_state_layout::resolve_project_origin(Path::new(&identity.source_root))?;
    ensure!(
        managed.verify_contained_workspace(&snapshot.binding.resource)?
            == managed.verify_contained_workspace(&origin)?,
        "native supervisor binding does not match the observed HTTP owner"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
