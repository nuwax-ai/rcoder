//! Finish a proxy withdrawal whose delayed stop left an inert registration.
use super::*;

impl SupervisordHost {
    /// Keep physical writes settled before inspecting the configuration and
    /// process table. A caller deadline never cancels a dispatched RPC/fsync.
    pub(super) async fn settle_entry_writes_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.client.wait_mutations_until(deadline).await?;
        crate::proxy::compiler::wait_for_file_replacements_until(deadline).await
    }

    pub(super) async fn reconcile_withdrawn_entry_until(
        &self,
        runtime_root: &Path,
        scope: &runtime_supervisor::ResidentScope,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.settle_entry_writes_until(deadline).await?;
        scope.verify_owner_for_cleanup()?;
        anyhow::ensure!(
            configured_entry_spec(&self.conf_path)?.is_none(),
            "withdrawn proxy gained a restart configuration during recovery"
        );
        let Some(info) = self.resident_entry_settled_until(deadline).await? else {
            return crate::proxy::compiler::clear_current_confirmation();
        };
        let state = info.checked_state(PINGAP_PROGRAM)?;
        anyhow::ensure!(
            matches!(
                state,
                SupervisorProcessState::Stopped
                    | SupervisorProcessState::Exited
                    | SupervisorProcessState::Fatal
            ) && info.pid == Some(0),
            "withdrawn proxy execution is not confirmed stopped: {}",
            info.brief()
        );
        if let Err(history) = verify_withdrawn_entry_receipt(
            runtime_root,
            scope.identity(),
            &crate::xmlrpc::default_socket_path(),
        ) {
            self.authorize_current_entry_domain_until(scope, deadline)
                .await
                .map_err(|current| {
                    anyhow::Error::new(EntryOwnershipUnconfirmed(format!(
                        "withdrawn proxy current domain is unconfirmed: {current:#}; historical receipt: {history:#}"
                    )))
                    .context(supervisor::ShutdownUnconfirmed(
                        "withdrawn proxy current ownership is unconfirmed; business retained".into(),
                    ))
                })?;
            self.preserve_resident_history_until(runtime_root, deadline)
                .await?;
        }
        crate::xmlrpc::require_deadline_budget("withdrawn proxy registration removal", deadline)?;
        mutation_result(
            self.client
                .remove_process_group_until(PINGAP_PROGRAM, deadline)
                .await,
        )?;
        anyhow::ensure!(
            self.resident_entry_settled_until(deadline).await?.is_none(),
            "withdrawn proxy registration remains after removal"
        );
        scope.verify_owner_for_cleanup()?;
        anyhow::ensure!(
            configured_entry_spec(&self.conf_path)?.is_none(),
            "withdrawn proxy restart configuration changed during removal"
        );
        crate::xmlrpc::require_deadline_budget("withdrawn proxy removal confirmation", deadline)?;
        crate::proxy::compiler::clear_current_confirmation()
    }
    /// A platform builder's current source/state binding, or an actual
    /// supervisord-managed owner in this process ancestry, authorizes its local
    /// inert registration. A missing historical claim does not revoke either.
    pub(super) async fn authorize_current_entry_domain_until(
        &self,
        scope: &runtime_supervisor::ResidentScope,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.verify_current_entry_context(scope)?;
        let workspace: PathBuf = serde_json::from_value(
            scope
                .identity()
                .binding
                .get("resource")
                .cloned()
                .context("current owner workspace binding missing")?,
        )?;
        let owner_root = scope
            .root()
            .parent()
            .and_then(Path::parent)
            .context("current owner state root missing")?;
        match runtime_state_layout::ManagedWorkspace::from_env(&workspace, owner_root) {
            Ok(Some(managed)) => {
                anyhow::ensure!(
                    managed.application_id == scope.identity().application_id,
                    "current platform builder belongs to another application"
                );
                crate::xmlrpc::require_deadline_budget(
                    "current builder domain authorization",
                    deadline,
                )?;
                return Ok(());
            }
            Ok(None) => {}
            Err(_) => tracing::warn!(app_id=%scope.identity().application_id,
                "current managed layout proof unavailable; observing the live supervisor owner"),
        }
        self.authorize_live_supervisor_owner_until(scope, deadline)
            .await
    }

    fn verify_current_entry_context(
        &self,
        scope: &runtime_supervisor::ResidentScope,
    ) -> Result<()> {
        scope.verify_owner_for_cleanup()?;
        if let Ok(project) = std::env::var("PROJECT_ID") {
            anyhow::ensure!(
                project.trim().is_empty() || project == scope.identity().application_id,
                "current platform PROJECT_ID belongs to another application"
            );
        }
        if let Some(current) = supervisor::resident::owner_scope()? {
            anyhow::ensure!(
                current.identity() == scope.identity(),
                "current resident owner workspace or instance differs from captured scope"
            );
        }
        let expected = crate::xmlrpc::default_socket_path();
        let same_socket = self.client.socket_path() == expected.as_path()
            || std::fs::canonicalize(self.client.socket_path())
                .ok()
                .zip(std::fs::canonicalize(&expected).ok())
                .is_some_and(|(left, right)| left == right);
        anyhow::ensure!(
            same_socket,
            "current supervisor socket differs from the selected owner engine"
        );
        Ok(())
    }

    /// Live proxy retirement needs a real supervisor/owner relationship even
    /// when a platform declaration identifies this private application.
    pub(super) async fn authorize_live_supervisor_owner_until(
        &self,
        scope: &runtime_supervisor::ResidentScope,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.verify_current_entry_context(scope)?;
        // Workers may be descendants of the serve process, rather than the
        // exact PID registered as app-cli. Observe live ancestry, never a PID
        // recovered from a journal or an old engine receipt.
        let infos = self.client.get_all_process_info_until(deadline).await?;
        let mut owners = infos.iter().filter(|info| info.group == "app-cli");
        let owner = owners
            .next()
            .context("local supervisor has no current app-cli owner")?;
        anyhow::ensure!(
            owners.next().is_none()
                && owner.checked_state("app-cli")? == SupervisorProcessState::Running,
            "local supervisor app-cli owner is not uniquely running"
        );
        let running = owner.running_identity()?;
        let pid = u32::try_from(running.pid)?;
        anyhow::ensure!(
            current_owner_ancestor(pid, deadline)?,
            "local supervisor manages another live app-cli owner"
        );
        let observed = self.client.process_info_until("app-cli", deadline).await?;
        anyhow::ensure!(
            observed.checked_state("app-cli")? == SupervisorProcessState::Running
                && observed.running_identity()? == running,
            "local supervisor owner changed during current domain observation"
        );
        anyhow::ensure!(
            current_owner_ancestor(pid, deadline)?,
            "current owner ancestry changed during supervisor domain observation"
        );
        scope.verify_owner_for_cleanup()?;
        crate::xmlrpc::require_deadline_budget("current supervisor domain authorization", deadline)
    }

    /// Preserve opaque old bytes inside this runtime's existing claim history.
    /// Never follow any configuration, process or workspace named by the data.
    pub(super) async fn preserve_resident_history_until(
        &self,
        runtime_root: &Path,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        if let Some(held) = supervisor::resident::owner_runtime_root()? {
            anyhow::ensure!(
                std::fs::canonicalize(held)? == std::fs::canonicalize(runtime_root)?,
                "current runtime root differs from the retained owner domain"
            );
        }
        let path = runtime_root.join(RESIDENT_ENGINE_RECEIPT);
        let bytes = match tokio::time::timeout_at(deadline, tokio::fs::read(&path))
            .await
            .context("read historical resident receipt deadline exceeded")?
        {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).context("preserve historical resident receipt"),
        };
        let history = runtime_root
            .join("resident-supervisord-claims")
            .join(format!("historical-{}.json", uuid::Uuid::new_v4()));
        tokio::time::timeout_at(
            deadline,
            crate::proxy::compiler::replace_file_bytes(&history, bytes),
        )
        .await
        .map_err(|_| {
            supervisor::ShutdownUnconfirmed(
                "historical resident receipt preservation remains unsettled at deadline".into(),
            )
        })??;
        tracing::warn!(target_path=%history.display(),
            "retained old resident receipt as diagnostic history under current owner authority");
        Ok(())
    }
}

fn current_owner_ancestor(owner: u32, deadline: tokio::time::Instant) -> Result<bool> {
    if owner == std::process::id() {
        return Ok(true);
    }
    #[cfg(target_os = "linux")]
    {
        let mut pid = std::process::id();
        let mut observed = Vec::new();
        for _ in 0..64 {
            crate::xmlrpc::require_deadline_budget("current owner ancestry observation", deadline)?;
            let identity = process_parent_and_start(pid)?;
            observed.push((pid, identity));
            if pid == owner {
                for (pid, identity) in observed {
                    anyhow::ensure!(
                        process_parent_and_start(pid)? == identity,
                        "owner process ancestry changed during observation"
                    );
                }
                return Ok(true);
            }
            if identity.0 == 0 || identity.0 == pid {
                return Ok(false);
            }
            pid = identity.0;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = deadline;
    Ok(false)
}

#[cfg(target_os = "linux")]
fn process_parent_and_start(pid: u32) -> Result<(u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .context("observe current owner process ancestry")?;
    let (_, fields) = stat
        .rsplit_once(") ")
        .context("current owner proc stat command delimiter missing")?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    let parent = fields
        .get(1)
        .context("current owner parent PID missing")?
        .parse()?;
    let started = fields
        .get(19)
        .context("current owner process start missing")?
        .parse()?;
    Ok((parent, started))
}
