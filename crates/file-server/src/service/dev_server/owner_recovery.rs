//! Recover the management process before reconciling stale transport registrations.
//! The owner itself proves exclusion and child cleanup; a refused TCP connection
//! only authorizes attempting bootstrap, never deleting runtime journals.
use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use shared_types::RuntimeIdentityView;

use super::{
    DevServerManager,
    owner_client::{self, OwnerClient, OwnerProbe},
    support::lock,
};
use crate::error::{AppError, AppResult};

pub(super) struct AvailableOwner {
    pub address: String,
    pub identity: RuntimeIdentityView,
}

impl DevServerManager {
    pub(super) async fn recover_owner_if_needed(
        &self,
        project: &str,
        workspace: &Path,
    ) -> AppResult<Option<AvailableOwner>> {
        self.recover_owner_inner(project, workspace)
            .await
            .map_err(|error| {
                AppError::business(format!("recover app-cli management service: {error:#}"))
            })
    }

    async fn recover_owner_inner(
        &self,
        project: &str,
        workspace: &Path,
    ) -> Result<Option<AvailableOwner>> {
        let snapshot = self.read_external_state()?;
        let address = snapshot.owner_address(project, &self.config.app_cli_admin_probe_addr);
        let Some(identity) = self
            .ensure_owner_available(project, workspace, address, snapshot.needs_owner(project))
            .await?
        else {
            return Ok(None);
        };
        if snapshot.has_replaced_registration(project, &identity.runtime_instance_id) {
            let app = std::env::var("PROJECT_ID")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "unknown-app".into());
            owner_client::verify_project_identity(&identity, workspace, &app)?;
            let (_, token) = owner_client::find_owner_token(Path::new(&identity.source_root), &app)
                .context("recovered owner control credentials unavailable")?;
            let client = OwnerClient::new(address, &token)?;
            let evidence = client
                .recovery()
                .await
                .context("successor startup cleanup is not confirmed")?;
            ensure!(
                evidence.runtime_instance_id == identity.runtime_instance_id,
                "owner changed while reading recovery evidence"
            );
            // No replay of old writes. Unknown deployment/migration results stay
            // in the authoritative owner's kernel and remain queryable there.
            self.retire_replaced_registration(project, &snapshot, &identity.runtime_instance_id)?;
        }
        Ok(Some(AvailableOwner {
            address: address.to_owned(),
            identity,
        }))
    }

    pub(super) async fn ensure_owner_available(
        &self,
        project: &str,
        workspace: &Path,
        address: &str,
        required: bool,
    ) -> Result<Option<RuntimeIdentityView>> {
        let observation = owner_client::observe_owner(address).await?;
        let identity = match observation {
            OwnerProbe::Ready(identity) => identity,
            OwnerProbe::Legacy => return Ok(None), // registered local run is stopped by its retained child
            OwnerProbe::Absent if !required => return Ok(None),
            OwnerProbe::Initializing if !required => {
                anyhow::bail!("owner is initializing; retry after startup completes")
            }
            OwnerProbe::Absent | OwnerProbe::Initializing => {
                let root = runtime_state_layout::ensure_state_root(
                    workspace,
                    std::env::var_os("APP_CLI_STATE_ROOT").as_deref(),
                    std::env::var_os("PROJECT_ID").as_deref(),
                )?;
                std::fs::create_dir_all(&root)?;
                let bootstrap = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(root.join("owner-bootstrap.lock"))?;
                bootstrap
                    .try_lock()
                    .context("owner bootstrap already in progress; retry after it completes")?;
                // Recheck after claiming bootstrap. app-cli also takes its own
                // authoritative OwnerGuard before binding or any runtime write.
                match owner_client::observe_owner(address).await? {
                    OwnerProbe::Ready(identity) => identity,
                    OwnerProbe::Legacy => return Ok(None),
                    observation => {
                        if matches!(observation, OwnerProbe::Absent) {
                            self.spawn_recovery_owner(project, workspace, address)
                                .await?;
                        }
                        self.wait_for_recovery_owner(project, address).await?
                    }
                }
            }
        };
        Ok(Some(identity))
    }

    async fn spawn_recovery_owner(
        &self,
        project: &str,
        workspace: &Path,
        address: &str,
    ) -> Result<()> {
        if lock(&self.owner_children)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .get(project)
            .is_some_and(|child| child.exited().is_none())
        {
            return Ok(());
        }
        // This helper starts locally; a remote discovery address cannot authorize
        // launching a different local coordinator.
        let address: std::net::SocketAddr = address
            .parse()
            .context("owner bootstrap requires a numeric loopback address")?;
        ensure!(
            address.ip().is_loopback(),
            "owner bootstrap requires a local loopback endpoint"
        );
        let directory = super::log::log_dir(&self.config, project).join("app-cli");
        tokio::fs::create_dir_all(&directory).await?;
        let log_path = directory.join("owner-recovery.log");
        let output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let mut command =
            tokio::process::Command::new(self.config.app_cli_bin.as_deref().unwrap_or("app-cli"));
        let bind = std::net::SocketAddr::new(
            if address.is_ipv4() {
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
            } else {
                std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
            },
            address.port(),
        );
        command
            .arg("serve")
            .arg("--control-only")
            .arg("--workspace")
            .arg(workspace)
            .arg("--log-dir")
            .arg(&directory)
            .arg("--admin-addr")
            .arg(bind.to_string())
            .current_dir(workspace)
            .stdin(std::process::Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output);
        #[cfg(unix)]
        command.process_group(0);
        let child = command
            .spawn()
            .context("spawn app-cli serve --control-only (requires matching app-cli build)")?;
        let tracked = super::SupervisedChild::adopt(
            child,
            Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        );
        lock(&self.owner_children)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .insert(project.into(), tracked);
        tracing::info!(%project, log = %log_path.display(), "started app-cli management recovery without business autostart");
        Ok(())
    }

    async fn wait_for_recovery_owner(
        &self,
        project: &str,
        address: &str,
    ) -> Result<RuntimeIdentityView> {
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                // Check the winner first: another managed supervisor can win the
                // OwnerGuard race while our control-only candidate exits.
                if let OwnerProbe::Ready(identity) = owner_client::observe_owner(address).await? {
                    return Ok(identity);
                }
                let exited = lock(&self.owner_children)
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .get(project)
                    .and_then(|child| child.exited());
                if let Some(exit) = exited {
                    anyhow::bail!(
                        "app-cli management recovery {}; see {}/app-cli/owner-recovery.log",
                        exit.describe(),
                        super::log::log_dir(&self.config, project).display()
                    );
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .context("app-cli management recovery timed out; original runtime state retained")?
    }

    /// Async preflight belongs before expensive build commands. Recheck at
    /// submission as well; this observation does not authorize a later write.
    pub async fn preflight_userapp_build(&self, project: &str, workspace: &Path) -> AppResult<()> {
        let owner = self.recover_owner_if_needed(project, workspace).await?;
        self.ensure_new_build_admissible(project)?;
        let address = owner
            .as_ref()
            .map(|owner| owner.address.as_str())
            .unwrap_or(&self.config.app_cli_admin_probe_addr);
        self.capture_owner_expectation_at(project, workspace, address)
            .await;
        if let Some(super::types::OwnerExpectation::ObservationFailed { reason }) =
            lock(&self.owner_expectations)?.get(project)
        {
            return Err(AppError::business(format!(
                "owner preflight failed: {reason}"
            )));
        }
        if let Some(owner) = owner {
            let identity = owner.identity;
            let app = identity.application_id.as_str();
            let (_, token) = owner_client::find_owner_token(Path::new(&identity.source_root), app)
                .ok_or_else(|| AppError::business("owner control credentials unavailable"))?;
            let evidence = async { OwnerClient::new(&owner.address, &token)?.recovery().await }
                .await
                .map_err(|e| AppError::business(format!("owner preflight: {e:#}")))?;
            if evidence.kernel_protected
                || (evidence.owner_protected && !evidence.credentials_required)
            {
                return Err(AppError::business(format!(
                    "runtime recovery required before build (operation {:?}, boundary {:?})",
                    evidence.operation_id, evidence.boundary
                )));
            }
        }
        Ok(())
    }
}
