use super::*;

impl DevServerManager {
    pub(crate) fn external_state_path(&self) -> std::path::PathBuf {
        self.config.log_base_dir.join("dev-server-external.json")
    }

    /// Lock is retained on disk. Losing contenders fail without waiting or side effects.
    pub(crate) fn external_transaction<T>(
        &self,
        mutate: impl FnOnce(&mut State) -> Result<T>,
    ) -> Result<T> {
        let path = self.external_state_path();
        let parent = path.parent().context("external state has no parent")?;
        std::fs::create_dir_all(parent).context("create external state directory")?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))
            .context("open external state lock")?;
        lock.try_lock().context("external owner state is busy")?;
        let mut state = read_or_quarantine(&path)?;
        let result = mutate(&mut state)?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).context("create external state transaction")?;
        serde_json::to_writer(&mut temporary, &state).context("encode external state")?;
        temporary.flush().context("flush external state")?;
        temporary
            .as_file()
            .sync_all()
            .context("sync external state")?;
        // tempfile uses atomic replacement, including replacement of existing files on Windows.
        temporary
            .persist(&path)
            .map_err(|error| error.error)
            .context("commit external state")?;
        #[cfg(unix)]
        std::fs::File::open(parent)?
            .sync_all()
            .context("sync external state directory")?;
        Ok(result)
    }

    pub(crate) fn read_external_state(&self) -> Result<State> {
        match read(&self.external_state_path()) {
            Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
                // Re-read under the transaction lock so a concurrent repair or
                // newly published registration cannot be overwritten.
                self.external_transaction(|state| Ok(state.clone()))
            }
            result => result,
        }
    }
    pub(crate) fn check_external_store(&self) -> Result<()> {
        self.read_external_state().map(|_| ())
    }

    /// Called after an authenticated successor has completed its own ownership
    /// and process cleanup checks. No port/PID observation can call this alone.
    pub(crate) fn retire_replaced_registration(
        &self,
        project: &str,
        snapshot: &State,
        successor: &str,
    ) -> Result<()> {
        let mut processes = self
            .processes
            .lock()
            .map_err(|_| anyhow::anyhow!("external process registry poisoned"))?;
        let mut stops = self
            .external_stops
            .lock()
            .map_err(|_| anyhow::anyhow!("external stop registry poisoned"))?;
        self.external_transaction(|state| {
            ensure!(state.project_snapshot(project) == snapshot.project_snapshot(project),
                "owner registration changed during recovery; retry with current state");
            // Never retire a request already issued to this successor.
            let slots: Vec<_> = state.intents.iter()
                .filter(|(slot, intent)| slot.starts_with(&format!("{project}|"))
                    && intent.request.expected_runtime_instance_id != successor)
                .map(|(slot, _)| slot.clone()).collect();
            let audit = state.project_snapshot(project);
            for slot in slots {
                if let Some(intent) = state.intents.remove(&slot) {
                    state.completed.insert(format!("{project}|{}", intent.request.operation_id), intent);
                }
            }
            if state.owners.get(project).is_some_and(|owner| owner.owner.runtime_instance_id != successor) {
                state.owners.remove(project);
                state.stops.remove(project);
            }
            state.retired.insert(format!("{project}|{}", uuid::Uuid::new_v4().simple()),
                serde_json::json!({"reason":"owner_replaced", "successor": successor, "registration":audit}));
            Ok(())
        })?;
        if processes
            .get(project)
            .and_then(|p| p.external_owner.as_ref())
            .is_some_and(|owner| owner.runtime_instance_id != successor)
        {
            processes.remove(project);
        }
        if snapshot
            .owners
            .get(project)
            .is_some_and(|owner| owner.owner.runtime_instance_id != successor)
        {
            stops.remove(project);
        }
        tracing::info!(%project, %successor, "retired old owner transport registration; operation history retained");
        Ok(())
    }

    /// Prepare both owner registration and original request before sending any authenticated write.
    pub(crate) fn prepare_external_intent(
        &self,
        project: &str,
        workspace: &Path,
        owner: &ExternalOwner,
        candidate: &RuntimeOperationRequest,
    ) -> Result<Intent> {
        ensure!(
            candidate.expected_runtime_instance_id == owner.runtime_instance_id,
            "captured runtime instance no longer matches owner"
        );
        // One local publication boundary; short disk transactions never span HTTP/await.
        let mut processes = self
            .processes
            .lock()
            .map_err(|_| anyhow::anyhow!("external process registry poisoned"))?;
        let intent = self.external_transaction(|state| {
            let root = runtime_state_layout::canonical_project_root(workspace);
            let slot = key(project, candidate.kind);
            if let Some(intent) = state.intents.get(&slot) {
                ensure!(
                    intent.project_root.as_ref() == Some(&root),
                    "pending operation workspace root differs"
                );
                let same_owner = intent.address == owner.address
                    && intent.request.expected_runtime_instance_id == owner.runtime_instance_id;
                let same_request = intent.request.operation_id == candidate.operation_id
                    || (candidate.request_context.is_some()
                        && intent.request.request_context == candidate.request_context);
                ensure!(
                    !same_request || same_owner,
                    "original runtime request belongs to a retired owner; query its result before replay"
                );
                if same_owner && same_request {
                    ensure!(
                        intent.request.workspace_id == candidate.workspace_id
                            && serde_json::to_value(&intent.request.profile)?
                                == serde_json::to_value(&candidate.profile)?,
                        "retry changed the runtime request profile"
                    );
                    return Ok(intent.clone());
                }
                // A new user action is allowed to replace the local transport
                // slot. Keep the old request queryable; the owner serializes
                // execution and a late response must not remove the new slot.
                let previous = intent.clone();
                state.completed.insert(
                    format!("{project}|{}", previous.request.operation_id),
                    previous,
                );
            }
            // Legacy stop observations are not an active runtime operation.
            state.stops.remove(project);
            let mut redacted = candidate.clone();
            redacted.run_config = None;
            let intent = Intent {
                request: redacted,
                digest: digest(candidate)?,
                has_private_config: candidate.run_config.is_some(),
                dispatch_pending: false,
                address: owner.address.clone(),
                project_root: Some(root),
            };
            state.owners.insert(
                project.to_string(),
                OwnerRecord {
                    pid: 0,
                    port: shared_types::APP_ENTRY_PORT,
                    project_id: project.to_string(),
                    registration_operation_id: Some(candidate.operation_id.clone()),
                    owner: OwnerIdentity {
                        address: owner.address.clone(),
                        runtime_instance_id: owner.runtime_instance_id.clone(),
                    },
                },
            );
            state.intents.insert(slot, intent.clone());
            Ok(intent)
        })?;
        processes.insert(
            project.to_string(),
            crate::models::DevProcess {
                pid: 0,
                port: shared_types::APP_ENTRY_PORT,
                project_id: project.to_string(),
                instance_id: None,
                base_path: None,
                started_at: 0,
                log_dir: crate::service::dev_server::log::log_dir(&self.config, project),
                temp_log_name: String::new(),
                external_owner: Some(owner.clone()),
            },
        );
        Ok(intent)
    }

    /// Old intents without dispatch evidence retain their original replay
    /// behavior. A recorded dispatched write is only observed, never repeated.
    pub(crate) async fn resume_external_intent_until(
        &self,
        project: &str,
        client: &OwnerClient,
        intent: &Intent,
        supplied: &RuntimeOperationRequest,
        deadline: tokio::time::Instant,
    ) -> Result<RuntimeOperationView> {
        process_utils::command_context::CommandContext::record_external_operation(
            &intent.request.operation_id,
            &intent.request.expected_runtime_instance_id,
            &intent.request.workspace_id,
        )
        .context(
            "persist local task to external operation identity before observation/submission",
        )?;
        if supplied.run_config.is_some() {
            let mut supplied_original = intent.request.clone();
            supplied_original.run_config = supplied.run_config.clone();
            ensure!(
                digest(&supplied_original)? == intent.digest,
                "runtime intent configuration differs; refusing to ignore new input"
            );
        }
        let current = self.read_external_state()?;
        let dispatched = current
            .intents
            .get(&key(project, intent.request.kind))
            .filter(|current| current.request.operation_id == intent.request.operation_id)
            .or_else(|| {
                current
                    .completed
                    .get(&format!("{project}|{}", intent.request.operation_id))
            })
            .is_some_and(|current| current.dispatch_pending);
        if dispatched {
            super::super::stop_activity::external_dispatched(&intent.request)?;
            return client
                .reconcile_dispatched_until(&intent.request, deadline)
                .await;
        }
        let observed = tokio::time::timeout_at(
            deadline,
            client.read_operation_if_exists(&intent.request.operation_id),
        )
        .await
        .context(
            "original runtime intent observation exceeded launch deadline; no POST dispatched",
        )??;
        ensure!(
            tokio::time::Instant::now() < deadline,
            "original runtime intent launch deadline expired before submission"
        );
        if let Some(view) = observed {
            OwnerClient::verify_request_view(&view, &intent.request)?;
            super::super::stop_activity::external_observed(&view)?;
            return Ok(view);
        }
        let mut original = intent.request.clone();
        if intent.has_private_config {
            ensure!(
                supplied.run_config.is_some(),
                "runtime intent needs the original private configuration to replay; recovery required"
            );
            original.run_config = supplied.run_config.clone();
        }
        ensure!(
            digest(&original)? == intent.digest,
            "runtime intent private configuration differs; refusing replay"
        );
        ensure!(
            tokio::time::Instant::now() < deadline,
            "original runtime intent launch deadline expired before submission"
        );
        let should_dispatch = self.external_transaction(|state| {
            let current = state
                .intents
                .get_mut(&key(project, original.kind))
                .context("runtime intent disappeared before dispatch")?;
            ensure!(
                current.request.operation_id == original.operation_id
                    && current.request.expected_runtime_instance_id
                        == original.expected_runtime_instance_id
                    && current.address == intent.address
                    && current.digest == intent.digest,
                "runtime intent changed before dispatch; refusing stale POST"
            );
            if current.dispatch_pending {
                return Ok(false);
            }
            current.dispatch_pending = true;
            Ok(true)
        })?;
        if !should_dispatch {
            super::super::stop_activity::external_dispatched(&original)?;
            return client.reconcile_dispatched_until(&original, deadline).await;
        }
        match client.submit_request_until(&original, deadline).await {
            Ok(view) => Ok(view),
            Err(error) => {
                if error.downcast_ref::<crate::service::dev_server::owner_client::SubmissionNotDispatched>().is_some() {
                    self.external_transaction(|state| {
                        let current = state.intents.get_mut(&key(project, original.kind))
                            .context("runtime intent disappeared before releasing undispatched request")?;
                        ensure!(current.request.operation_id == original.operation_id
                            && current.request.expected_runtime_instance_id == original.expected_runtime_instance_id,
                            "runtime intent changed before releasing undispatched request");
                        current.dispatch_pending = false;
                        Ok(())
                    })?;
                    super::super::stop_activity::external_not_admitted(&original.operation_id)?;
                }
                if error
                    .downcast_ref::<crate::service::dev_server::owner_client::SubmissionRejected>()
                    .is_some_and(|rejection| rejection.proves_not_admitted())
                {
                    super::super::stop_activity::external_not_admitted(
                        &intent.request.operation_id,
                    )?;
                    self.finish_external_intent(project, &intent.request, false)?;
                }
                Err(error)
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn resume_external_intent(
        &self,
        project: &str,
        client: &OwnerClient,
        intent: &Intent,
        supplied: &RuntimeOperationRequest,
    ) -> Result<RuntimeOperationView> {
        self.resume_external_intent_until(
            project,
            client,
            intent,
            supplied,
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .await
    }

    /// Never clear a replacement intent by project ID alone.
    pub(crate) fn finish_external_intent(
        &self,
        project: &str,
        request: &RuntimeOperationRequest,
        remove_owner: bool,
    ) -> Result<()> {
        let mut processes = self
            .processes
            .lock()
            .map_err(|_| anyhow::anyhow!("external process registry poisoned"))?;
        let removed = self.external_transaction(|state| {
            let slot = key(project, request.kind);
            let completed_key = format!("{project}|{}", request.operation_id);
            if state.completed.get(&completed_key).is_some_and(|old| {
                old.request.expected_runtime_instance_id == request.expected_runtime_instance_id
            }) {
                let remove_registration = remove_owner
                    && state.owners.get(project).is_some_and(|owner| {
                        owner.registration_operation_id.as_deref()
                            == Some(request.operation_id.as_str())
                            && owner.owner.runtime_instance_id
                                == request.expected_runtime_instance_id
                    });
                if remove_registration {
                    state.owners.remove(project);
                }
                return Ok(remove_registration);
            }
            let current = state
                .intents
                .get(&slot)
                .context("runtime intent disappeared; recovery required")?;
            ensure!(
                current.request.operation_id == request.operation_id
                    && current.request.expected_runtime_instance_id
                        == request.expected_runtime_instance_id,
                "runtime intent changed; refusing stale completion"
            );
            let completed = current.clone();
            state.intents.remove(&slot);
            state.completed.insert(completed_key, completed);
            let remove_registration = remove_owner
                && state.owners.get(project).is_some_and(|owner| {
                    owner.registration_operation_id.as_deref()
                        == Some(request.operation_id.as_str())
                        && owner.owner.runtime_instance_id == request.expected_runtime_instance_id
                });
            if remove_registration {
                state.owners.remove(project);
            }
            Ok(remove_registration)
        })?;
        if removed {
            processes.remove(project);
        }
        Ok(())
    }
}

pub(crate) fn verify_view(
    view: &RuntimeOperationView,
    request: &RuntimeOperationRequest,
) -> Result<()> {
    ensure!(
        view.operation_id == request.operation_id
            && view.runtime_instance_id == request.expected_runtime_instance_id
            && view.kind == request.kind,
        "runtime operation identity mismatch; recovery required"
    );
    Ok(())
}
