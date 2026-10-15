//! Retire an owned management process through native control, without reusing
//! an obsolete wire protocol or changing a live process's credentials.
use super::*;
use runtime_state_layout::ManagedWorkspace;
use runtime_supervisor::{Action, Intent, Phase, Request, Snapshot};

pub(super) fn known_legacy_protocol(identity: &RuntimeIdentityView) -> bool {
    identity.protocol_version == 4
}

pub(super) fn retirement_protocol_supported(identity: &RuntimeIdentityView) -> bool {
    owner_client::protocol_compatible(identity) || known_legacy_protocol(identity)
}

impl DevServerManager {
    pub(super) async fn restore_owner_metadata_before_start(
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
        ensure!(
            retirement_protocol_supported(&available.identity),
            "unsupported owner protocol for native retirement"
        );
        ensure!(
            available.identity.application_id == app
                && available.identity.service_family == "userapp-dev",
            "different app-cli owner: project or application identity mismatch"
        );
        let origin = runtime_state_layout::resolve_project_origin(workspace)?;
        let root = runtime_state_layout::resolve_state_root(
            workspace,
            std::env::var_os("APP_CLI_STATE_ROOT").as_deref(),
            (app != "unknown-app")
                .then(|| std::ffi::OsString::from(&app))
                .as_deref(),
        )?
        .context("native owner retirement has no declared state authority")?;
        let authority = match ManagedWorkspace::from_env(workspace, &root)? {
            Some(managed) => managed,
            None => ManagedWorkspace {
                application_id: app.clone(),
                source_root: std::fs::canonicalize(origin)
                    .context("resolve native retirement source root")?,
                state_root: root,
                single_application: false,
            },
        };
        // The retained identity.json, native binding, exact generation, and
        // owner lease are checked by the existing handover before any spawn.
        let next = tokio::time::timeout_at(
            deadline,
            self.handover_managed_owner(
                project,
                &available.address,
                available.identity,
                &authority,
            ),
        )
        .await
        .context("native owner retirement remains unsettled; retry the original request")??;
        owner_client::verify_project_identity(&next, workspace, &app)?;
        ensure!(
            next.capabilities.iter().any(|capability| {
                capability == shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY
            }),
            "restored management owner lacks the current proxy restart contract"
        );
        let (next_root, _) = owner_client::find_owner_token(Path::new(&next.source_root), &app)
            .context("restored management owner credentials are unavailable")?;
        ensure!(
            next_root == authority.state_root,
            "restored management owner changed the application state authority"
        );
        Ok(AvailableOwner {
            address: available.address,
            identity: next,
        })
    }
}

/// A legacy wildcard request is never dispatched again. Confirm its already
/// completed outcome under the original owner lease, including worker leases.
pub(super) async fn confirm_legacy_completed_shutdown(
    root: &Path,
    before: &Snapshot,
    request: &Request,
    deadline: tokio::time::Instant,
) -> Result<()> {
    ensure!(
        request.action == Action::Shutdown && request.expected_generation.is_none(),
        "legacy retirement requires the original completed Shutdown"
    );
    loop {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "original owner or worker lease has not settled before the retirement deadline"
        );
        verify_legacy_terminal(before, &runtime_supervisor::last_snapshot(root)?)?;
        let owner = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("owner.lock"))
            .context("observe existing owner lease for legacy retirement")?;
        match owner.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            Err(error) => return Err(error).context("observe legacy owner lease release"),
        }
        let observed = runtime_supervisor::last_snapshot(root)?;
        verify_legacy_terminal(before, &observed)?;
        let terminal = runtime_supervisor::saved_request_snapshot(root, request)?
            .context("original legacy Shutdown completion is unavailable")?;
        verify_legacy_terminal(before, &terminal)?;
        ensure!(
            terminal.operation_id.as_deref() == Some(request.request_id.as_str()),
            "original legacy Shutdown request identity changed"
        );
        let mut workers = Vec::new();
        let entries = match std::fs::read_dir(root.join("work")) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("observe legacy worker leases"),
        };
        let mut busy = false;
        for entry in entries.into_iter().flatten() {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            let worker = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path.join("generation.lock"))
            {
                Ok(worker) => worker,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context("observe existing worker lease"),
            };
            match worker.try_lock() {
                Ok(()) => workers.push(worker),
                Err(std::fs::TryLockError::WouldBlock) => {
                    busy = true;
                    break;
                }
                Err(error) => return Err(error).context("observe legacy worker lease release"),
            }
        }
        if busy {
            drop(workers);
            drop(owner);
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        // The native aggregate Stopped receipt proves command/engine cleanup;
        // free OS leases independently prove the old owners have exited. Old
        // command/migration diagnostics remain untouched, never replayed here.
        if let Some(generation) = before.generation.as_deref() {
            runtime_supervisor::verify_local_quiescent(root, generation)?
                .context("legacy Shutdown cleanup belongs to another process domain")?;
        }
        verify_legacy_terminal(before, &runtime_supervisor::last_snapshot(root)?)?;
        let repeated = runtime_supervisor::saved_request_snapshot(root, request)?
            .context("legacy Shutdown completion changed during lease observation")?;
        verify_legacy_terminal(before, &repeated)?;
        ensure!(
            repeated.operation_id.as_deref() == Some(request.request_id.as_str()),
            "original legacy Shutdown completion changed during lease observation"
        );
        return Ok(());
    }
}

fn verify_legacy_terminal(before: &Snapshot, observed: &Snapshot) -> Result<()> {
    if observed.supervisor_id != before.supervisor_id
        || observed.binding != before.binding
        || observed.phase != Phase::Stopped
        || observed.intent != Intent::Shutdown
        || (observed.generation != before.generation && observed.generation.is_some())
    {
        return Err(runtime_supervisor::Problem {
            code: runtime_supervisor::FailureCode::IdentityChanged,
            message: "original legacy owner identity or completed Shutdown changed".into(),
        }
        .into());
    }
    Ok(())
}
