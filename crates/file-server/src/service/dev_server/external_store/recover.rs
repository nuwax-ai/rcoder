use super::*;

impl DevServerManager {
    /// Historical transport intents do not own the workspace. The runtime
    /// owner handles active execution when the new build is submitted.
    pub fn ensure_new_build_admissible(&self, _project: &str) -> crate::error::AppResult<()> {
        self.check_external_store().map_err(|error| {
            crate::error::AppError::business(format!("read external runtime state: {error:#}"))
        })
    }

    /// A local child must still be owned, and the management API must confirm the same release is Ready.
    /// External registration alone never enables this fast path.
    pub async fn confirmed_local_manifest_ready(
        &self,
        project: &str,
        workspace: &Path,
    ) -> crate::error::AppResult<bool> {
        use crate::error::AppError;
        let state = self
            .read_external_state()
            .map_err(|e| AppError::business(format!("external recovery required: {e:#}")))?;
        if state
            .intents
            .contains_key(&key(project, RuntimeOperationKind::Restart))
            || state
                .intents
                .contains_key(&key(project, RuntimeOperationKind::Deploy))
            || state
                .intents
                .contains_key(&key(project, RuntimeOperationKind::Stop))
        {
            return Ok(false);
        }
        let process = crate::service::dev_server::support::lock(&self.processes)?
            .get(project)
            .cloned();
        let Some(process) = process.filter(|p| p.external_owner.is_none()) else {
            return Ok(false);
        };
        let child = crate::service::dev_server::support::lock(&self.supervised)?
            .get(project)
            .cloned();
        let Some(child) = child.filter(|c| c.pid() == process.pid && c.exited().is_none()) else {
            return Ok(false);
        };
        let Some(release_id) = tokio::fs::read_to_string(workspace.join("release.lock.toml"))
            .await
            .ok()
            .and_then(|text| shared_types::load_release_lock(&text).ok())
            .map(|lock| lock.release_id)
        else {
            return Ok(false);
        };
        let probe = async {
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(3))
                .build()?;
            let base = format!("http://{}", self.config.app_cli_admin_probe_addr);
            let status: serde_json::Value = client
                .get(format!("{base}/v1/deploy/status"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let ready: serde_json::Value = client
                .get(format!("{base}/ready"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            Ok::<_, reqwest::Error>(
                status["data"]["release_id"].as_str() == Some(release_id.as_str())
                    && status["data"]["phase"] == "running"
                    && ready["phase"] == "running"
                    && ready["status"] == "ready",
            )
        }
        .await;
        Ok(probe.unwrap_or(false) && child.exited().is_none())
    }

    pub fn external_operation_for_task(
        &self,
        project: &str,
        task_id: &str,
    ) -> crate::error::AppResult<Option<String>> {
        let state = self.read_external_state().map_err(|e| {
            crate::error::AppError::business(format!("external recovery required: {e:#}"))
        })?;
        Ok(state
            .intents
            .iter()
            .chain(state.completed.iter())
            .find(|(slot, intent)| {
                slot.starts_with(&format!("{project}|"))
                    && intent.request.request_context.as_deref() == Some(task_id)
            })
            .map(|(_, intent)| intent.request.operation_id.clone()))
    }

    /// Explicit recovery never rebuilds source or changes the captured operation/version.
    pub async fn recover_external_operation(
        &self,
        project: &str,
        workspace: &Path,
        operation_id: &str,
        pg: Option<&shared_types::StartPgCredential>,
    ) -> crate::error::AppResult<(Option<String>, RuntimeOperationView)> {
        let recover = async {
            let state = self.read_external_state()?;
            let intent = state
                .intents
                .iter()
                .chain(state.completed.iter())
                .find(|(slot, intent)| {
                    slot.starts_with(&format!("{project}|"))
                        && intent.request.operation_id == operation_id
                })
                .map(|(_, intent)| intent.clone())
                .context("runtime operation not found for this application")?;
            ensure!(
                intent.project_root.as_ref()
                    == Some(&runtime_state_layout::canonical_project_root(workspace)),
                "recovery workspace differs from captured project"
            );
            let completed = state
                .completed
                .contains_key(&format!("{project}|{operation_id}"));
            // Restore only the management channel here. The caller asked to
            // recover this exact operation, so preserve its pending receipt
            // until the matching durable result below has been verified.
            let identity = self
                .ensure_owner_available(project, workspace, &intent.address, true)
                .await?
                .context("runtime owner unavailable; recovery remains protected")?;
            let app = std::env::var("PROJECT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "unknown-app".into());
            crate::service::dev_server::owner_client::verify_project_identity(
                &identity, workspace, &app,
            )?;
            ensure!(
                identity.workspace_id == intent.request.workspace_id,
                "runtime workspace changed; cannot recover old operation"
            );
            let (_, token) = crate::service::dev_server::owner_client::find_owner_token(
                Path::new(&identity.source_root),
                &app,
            )
            .context("runtime owner credentials unavailable")?;
            let client = OwnerClient::new(&intent.address, &token)?;
            let mut supplied = intent.request.clone();
            supplied.run_config = pg.map(|pg| shared_types::OperationRunConfig {
                pg: Some(pg.clone()),
            });
            let owner_changed =
                identity.runtime_instance_id != intent.request.expected_runtime_instance_id;
            // A new process may serve the old process's durable terminal history.
            // Neither owner replacement nor completed receipts authorize replaying old writes.
            let view = if completed || owner_changed {
                if supplied.run_config.is_some() {
                    ensure!(
                        digest(&supplied)? == intent.digest,
                        "runtime intent configuration differs"
                    );
                }
                let view = client.operation_if_exists(operation_id).await?.context(
                    "completed operation no longer retained by owner; replay prohibited",
                )?;
                verify_view(&view, &intent.request)?;
                if owner_changed {
                    ensure!(
                        view.state.is_terminal(),
                        "old owner operation is not confirmed terminal; recovery remains protected"
                    );
                    let expected_digest = shared_types::runtime_request_digest(&intent.request)
                        .map_err(anyhow::Error::msg)?;
                    ensure!(
                        view.request_digest == expected_digest,
                        "old owner operation digest mismatch; recovery remains protected"
                    );
                }
                view
            } else {
                self.resume_external_intent(project, &client, &intent, &supplied)
                    .await?
            };
            if (!completed || owner_changed)
                && matches!(
                    view.state,
                    shared_types::RuntimeOperationState::Succeeded
                        | shared_types::RuntimeOperationState::Failed
                        | shared_types::RuntimeOperationState::Cancelled
                )
            {
                self.finish_external_intent(
                    project,
                    &intent.request,
                    owner_changed
                        || (view.kind == RuntimeOperationKind::Stop
                            && view.state == shared_types::RuntimeOperationState::Succeeded),
                )?;
            }
            Ok::<_, anyhow::Error>((intent.request.request_context, view))
        }
        .await;
        recover.map_err(|error| {
            crate::error::AppError::owner_error("runtime operation recovery", error)
        })
    }
}
