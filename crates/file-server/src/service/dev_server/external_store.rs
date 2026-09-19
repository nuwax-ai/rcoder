//! Durable external-owner intents. File transactions are short and never span HTTP.
use std::{collections::HashMap, fs::OpenOptions, io::Write, path::Path};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shared_types::{RuntimeOperationKind, RuntimeOperationRequest, RuntimeOperationView};

use super::{owner_client::OwnerClient, types::DevServerManager};
use crate::models::ExternalOwner;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Intent {
    pub request: RuntimeOperationRequest,
    /// Digest of the original full request, including private configuration.
    digest: String,
    has_private_config: bool,
    address: String,
    #[serde(default)]
    project_root: Option<std::path::PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct OwnerRecord {
    pub pid: u32,
    pub port: u16,
    pub project_id: String,
    pub owner: OwnerIdentity,
    #[serde(default)]
    pub registration_operation_id: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct OwnerIdentity {
    pub address: String,
    pub runtime_instance_id: String,
}
#[derive(Default, Serialize, Deserialize)]
pub(super) struct State {
    pub owners: HashMap<String, OwnerRecord>,
    pub stops: HashMap<String, super::types::ExternalStopRecord>,
    #[serde(default)]
    pub intents: HashMap<String, Intent>,
    #[serde(default)]
    completed: HashMap<String, Intent>,
}

fn key(project: &str, kind: RuntimeOperationKind) -> String {
    format!("{project}|{kind:?}")
}
fn digest(request: &RuntimeOperationRequest) -> Result<String> {
    let bytes = serde_json::to_vec(request).context("encode runtime intent digest")?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
fn read(path: &Path) -> Result<State> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("external owner state is corrupt; recovery required"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error).context("read external owner state; recovery required"),
    }
}

impl DevServerManager {
    pub(super) fn external_state_path(&self) -> std::path::PathBuf {
        self.config.log_base_dir.join("dev-server-external.json")
    }

    /// Lock is retained on disk. Losing contenders fail without waiting or side effects.
    pub(super) fn external_transaction<T>(
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
        let mut state = read(&path)?;
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

    pub(super) fn read_external_state(&self) -> Result<State> {
        read(&self.external_state_path())
    }
    pub(super) fn check_external_store(&self) -> Result<()> {
        self.read_external_state().map(|_| ())
    }

    /// Prepare both owner registration and original request before sending any authenticated write.
    pub(super) fn prepare_external_intent(
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
                ensure!(intent.project_root.as_ref() == Some(&root), "pending operation workspace root differs; recovery required");
                ensure!(
                    intent.address == owner.address
                        && intent.request.expected_runtime_instance_id == owner.runtime_instance_id,
                    "pending runtime intent belongs to a different owner; recovery required"
                );
                ensure!(
                    intent.request.workspace_id == candidate.workspace_id
                        && serde_json::to_value(&intent.request.profile)?
                            == serde_json::to_value(&candidate.profile)?,
                    "pending runtime intent has a different profile; recovery required"
                );
                if candidate.kind == RuntimeOperationKind::Restart {
                    ensure!(intent.request.request_context == candidate.request_context
                        && (candidate.request_context.is_some() || intent.request.operation_id == candidate.operation_id),
                        "another build request has pending operation {}; resume that operation before a new build request", intent.request.operation_id);
                }
                return Ok(intent.clone());
            }
            if candidate.kind == RuntimeOperationKind::Stop && state.stops.contains_key(project) {
                bail!("legacy stop intent requires query-only recovery");
            }
            let mut redacted = candidate.clone();
            redacted.run_config = None;
            let intent = Intent {
                request: redacted,
                digest: digest(candidate)?,
                has_private_config: candidate.run_config.is_some(),
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
                log_dir: super::log::log_dir(&self.config, project),
                temp_log_name: String::new(),
                external_owner: Some(owner.clone()),
            },
        );
        Ok(intent)
    }

    /// Query first; only a definite 404 authorizes replay of the identical original request.
    pub(super) async fn resume_external_intent(
        &self,
        project: &str,
        client: &OwnerClient,
        intent: &Intent,
        supplied: &RuntimeOperationRequest,
    ) -> Result<RuntimeOperationView> {
        if supplied.run_config.is_some() {
            let mut supplied_original = intent.request.clone();
            supplied_original.run_config = supplied.run_config.clone();
            ensure!(
                digest(&supplied_original)? == intent.digest,
                "runtime intent configuration differs; refusing to ignore new input"
            );
        }
        if let Some(view) = client
            .operation_if_exists(&intent.request.operation_id)
            .await?
        {
            verify_view(&view, &intent.request)?;
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
        match client.submit_request(&original).await {
            Ok(view) => Ok(view),
            Err(error) => {
                if error
                    .downcast_ref::<super::owner_client::SubmissionRejected>()
                    .is_some_and(|rejection| rejection.proves_not_admitted())
                {
                    self.finish_external_intent(project, &intent.request, false)?;
                }
                Err(error)
            }
        }
    }

    /// Never clear a replacement intent by project ID alone.
    pub(super) fn finish_external_intent(
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
            if !state.intents.contains_key(&slot)
                && state.completed.get(&completed_key).is_some_and(|old| {
                    old.request.expected_runtime_instance_id == request.expected_runtime_instance_id
                })
            {
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

pub(super) fn verify_view(
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

impl DevServerManager {
    pub fn ensure_new_build_admissible(&self, project: &str) -> crate::error::AppResult<()> {
        let state = self.read_external_state().map_err(|e| {
            crate::error::AppError::business(format!("external recovery required: {e:#}"))
        })?;
        if let Some(intent) = state
            .intents
            .get(&key(project, RuntimeOperationKind::Restart))
            .or_else(|| state.intents.get(&key(project, RuntimeOperationKind::Stop)))
        {
            return Err(crate::error::AppError::Conflict(format!(
                "pending runtime operation {}; recover this operation before creating another build task",
                intent.request.operation_id
            )));
        }
        Ok(())
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
                .contains_key(&key(project, RuntimeOperationKind::Stop))
        {
            return Ok(false);
        }
        let process = super::support::lock(&self.processes)?.get(project).cloned();
        let Some(process) = process.filter(|p| p.external_owner.is_none()) else {
            return Ok(false);
        };
        let child = super::support::lock(&self.supervised)?
            .get(project)
            .cloned();
        let Some(child) = child.filter(|c| c.pid() == process.pid && c.exited().is_none()) else {
            return Ok(false);
        };
        let Some(release_id) = tokio::fs::read_to_string(workspace.join("release.lock.toml"))
            .await
            .ok()
            .and_then(|text| text.parse::<toml::Value>().ok())
            .and_then(|value| {
                value
                    .get("release_id")
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
            })
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
            let identity = super::owner_client::probe_owner(&intent.address)
                .await
                .context("runtime owner unavailable; recovery remains protected")?;
            let app = std::env::var("PROJECT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "unknown-app".into());
            super::owner_client::verify_project_identity(&identity, workspace, &app)?;
            ensure!(
                identity.workspace_id == intent.request.workspace_id,
                "runtime workspace changed; cannot recover old operation"
            );
            let (_, token) = super::owner_client::find_owner_token(workspace, &app)
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
            crate::error::AppError::business(format!("runtime operation recovery: {error:#}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::{RunProfileInput, RuntimeOperationState};
    use std::sync::{Arc, Mutex};

    fn manager(root: &Path) -> DevServerManager {
        let mut config = crate::Config::from_env().unwrap();
        config.log_base_dir = root.to_path_buf();
        DevServerManager::new(Arc::new(config))
    }
    fn request() -> RuntimeOperationRequest {
        RuntimeOperationRequest {
            operation_id: "original-operation".into(),
            expected_runtime_instance_id: "instance".into(),
            expected_revision: 7,
            workspace_id: "workspace".into(),
            kind: RuntimeOperationKind::Restart,
            profile: RunProfileInput::Source {
                workspace_id: "workspace".into(),
            },
            run_config: None,
            request_context: Some("build-task-one".into()),
        }
    }
    fn owner(address: &str) -> ExternalOwner {
        ExternalOwner {
            address: address.into(),
            token: "private-owner-token".into(),
            runtime_instance_id: "instance".into(),
        }
    }
    #[test]
    fn pending_task_context_cannot_be_replaced_by_new_or_anonymous_request() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let original = request();
        let owner = owner("127.0.0.1:1");
        manager
            .prepare_external_intent("project", dir.path(), &owner, &original)
            .unwrap();
        for context in [Some("another-task".to_owned()), None] {
            let mut candidate = original.clone();
            candidate.request_context = context;
            candidate.operation_id = "replacement-operation".into();
            let error = manager
                .prepare_external_intent("project", dir.path(), &owner, &candidate)
                .err()
                .unwrap();
            assert!(error.to_string().contains(&original.operation_id));
            let state = manager.read_external_state().unwrap();
            assert_eq!(
                state.intents[&key("project", original.kind)]
                    .request
                    .operation_id,
                original.operation_id
            );
        }
        let mut same_task = original.clone();
        same_task.operation_id = "discarded-new-id".into();
        same_task.expected_revision = 100;
        let resumed = manager
            .prepare_external_intent("project", dir.path(), &owner, &same_task)
            .unwrap();
        assert_eq!(resumed.request.operation_id, original.operation_id);
        assert_eq!(
            resumed.request.expected_revision,
            original.expected_revision
        );
    }

    #[test]
    fn anonymous_retry_requires_original_operation_identity() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let mut original = request();
        original.request_context = None;
        let owner = owner("127.0.0.1:1");
        manager
            .prepare_external_intent("project", dir.path(), &owner, &original)
            .unwrap();
        assert!(
            manager
                .prepare_external_intent("project", dir.path(), &owner, &original)
                .is_ok()
        );
        let mut candidate = original.clone();
        candidate.operation_id = "another-operation".into();
        assert!(
            manager
                .prepare_external_intent("project", dir.path(), &owner, &candidate)
                .is_err()
        );
    }

    #[test]
    fn stale_stop_completion_preserves_new_registration_on_same_instance() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let owner = owner("127.0.0.1:1");
        let mut stop = request();
        stop.kind = RuntimeOperationKind::Stop;
        stop.operation_id = "old-stop".into();
        manager
            .prepare_external_intent("project", dir.path(), &owner, &stop)
            .unwrap();
        let mut restart = request();
        restart.operation_id = "new-restart".into();
        manager
            .prepare_external_intent("project", dir.path(), &owner, &restart)
            .unwrap();
        manager
            .finish_external_intent("project", &restart, false)
            .unwrap();
        manager
            .finish_external_intent("project", &stop, true)
            .unwrap();
        assert_eq!(
            manager.read_external_state().unwrap().owners["project"]
                .registration_operation_id
                .as_deref(),
            Some("new-restart")
        );
        assert!(manager.processes.lock().unwrap().contains_key("project"));
        // Re-observing old completion also cannot clear newer registry state.
        manager
            .finish_external_intent("project", &stop, true)
            .unwrap();
        assert!(manager.processes.lock().unwrap().contains_key("project"));
        stop.operation_id = "current-stop".into();
        manager
            .prepare_external_intent("project", dir.path(), &owner, &stop)
            .unwrap();
        manager
            .finish_external_intent("project", &stop, true)
            .unwrap();
        assert!(
            !manager
                .read_external_state()
                .unwrap()
                .owners
                .contains_key("project")
        );
        assert!(!manager.processes.lock().unwrap().contains_key("project"));
    }

    #[tokio::test]
    async fn restarted_owner_only_closes_matching_terminal_history_without_replaying() {
        for scenario in [
            "succeeded",
            "failed",
            "cancelled",
            "missing",
            "unknown",
            "wrong_instance",
            "wrong_kind",
            "wrong_digest",
            "wrong_workspace",
            "new_registration",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = dir.path().join("workspace");
            std::fs::create_dir_all(&workspace).unwrap();
            let application = std::env::var("PROJECT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "unknown-app".into());
            let token_root = dir.path().join(".app-cli-state").join(&application);
            std::fs::create_dir_all(&token_root).unwrap();
            std::fs::write(token_root.join("token"), "receipt-token").unwrap();
            let original = request();
            let mut receipt = RuntimeOperationView {
                operation_id: original.operation_id.clone(),
                kind: original.kind,
                state: RuntimeOperationState::Succeeded,
                request_digest: shared_types::runtime_request_digest(&original).unwrap(),
                revision: 8,
                runtime_instance_id: original.expected_runtime_instance_id.clone(),
                error_code: None,
                error_message: None,
                failure_detail: None,
            };
            match scenario {
                "failed" => receipt.state = RuntimeOperationState::Failed,
                "cancelled" => receipt.state = RuntimeOperationState::Cancelled,
                "unknown" => receipt.state = RuntimeOperationState::RecoveryRequired,
                "wrong_instance" => receipt.runtime_instance_id = "unrelated-owner".into(),
                "wrong_kind" => receipt.kind = RuntimeOperationKind::Stop,
                "wrong_digest" => receipt.request_digest = "wrong".into(),
                _ => {}
            }
            let identity = shared_types::RuntimeIdentityView {
                application_id: application,
                service_family: "userapp-dev".into(),
                workspace_id: if scenario == "wrong_workspace" {
                    "other-workspace"
                } else {
                    "workspace"
                }
                .into(),
                source_root: workspace.to_string_lossy().into(),
                runtime_instance_id: "replacement-owner".into(),
                deployment_generation_id: "generation".into(),
                protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
                capabilities: vec![],
            };
            let posts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let posted = posts.clone();
            let server = axum::Router::new()
                .route(
                    "/v1/runtime/identity",
                    axum::routing::get(move || {
                        let identity = identity.clone();
                        async move { axum::Json(serde_json::json!({"data":identity})) }
                    }),
                )
                .route(
                    "/v1/runtime/operations/{id}",
                    axum::routing::get(move |headers: axum::http::HeaderMap| {
                        let receipt = receipt.clone();
                        async move {
                            assert_eq!(headers["x-deploy-token"], "receipt-token");
                            if scenario == "missing" {
                                return (
                                    axum::http::StatusCode::NOT_FOUND,
                                    axum::Json(serde_json::json!({})),
                                );
                            }
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({"data": receipt})),
                            )
                        }
                    }),
                )
                .route(
                    "/v1/runtime/operations",
                    axum::routing::post(move || {
                        let posted = posted.clone();
                        async move {
                            posted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let server = tokio::spawn(async move {
                axum::serve(listener, server).await.unwrap();
            });
            let manager = manager(dir.path());
            manager
                .prepare_external_intent("project", &workspace, &owner(&address), &original)
                .unwrap();
            if scenario == "new_registration" {
                // Another admitted operation has already registered the replacement instance.
                let mut stop = original.clone();
                stop.operation_id = "new-stop".into();
                stop.kind = RuntimeOperationKind::Stop;
                stop.expected_runtime_instance_id = "replacement-owner".into();
                let mut replacement = owner(&address);
                replacement.runtime_instance_id = "replacement-owner".into();
                manager
                    .prepare_external_intent("project", &workspace, &replacement, &stop)
                    .unwrap();
            }
            let result = manager
                .recover_external_operation("project", &workspace, &original.operation_id, None)
                .await;
            let success = matches!(
                scenario,
                "succeeded" | "failed" | "cancelled" | "new_registration"
            );
            assert_eq!(result.is_ok(), success, "scenario {scenario}: {result:?}");
            assert_eq!(
                posts.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "old writes must never be replayed"
            );
            let state = manager.read_external_state().unwrap();
            assert_eq!(
                state.intents.contains_key(&key("project", original.kind)),
                !success
            );
            if scenario == "new_registration" {
                assert_eq!(
                    state.owners["project"].registration_operation_id.as_deref(),
                    Some("new-stop")
                );
                assert_eq!(
                    manager.processes.lock().unwrap()["project"]
                        .external_owner
                        .as_ref()
                        .unwrap()
                        .runtime_instance_id,
                    "replacement-owner"
                );
            } else if success {
                assert!(!state.owners.contains_key("project"));
                assert!(!manager.processes.lock().unwrap().contains_key("project"));
                manager.ensure_new_build_admissible("project").unwrap();
                // Re-querying the completed receipt remains read-only and idempotent.
                assert!(
                    manager
                        .recover_external_operation(
                            "project",
                            &workspace,
                            &original.operation_id,
                            None
                        )
                        .await
                        .is_ok()
                );
            } else {
                assert!(manager.ensure_new_build_admissible("project").is_err());
            }
            server.abort();
        }
    }

    #[derive(Default)]
    struct Wire {
        posts: Vec<RuntimeOperationRequest>,
        committed: bool,
    }
    async fn mock(
        root: std::path::PathBuf,
        fail_first: bool,
        accept_before_failure: bool,
    ) -> (String, Arc<Mutex<Wire>>, tokio::task::JoinHandle<()>) {
        let wire = Arc::new(Mutex::new(Wire::default()));
        let posted = wire.clone();
        let queried = wire.clone();
        let router = axum::Router::new()
            .route("/v1/runtime/operations", axum::routing::post(move |axum::Json(req): axum::Json<RuntimeOperationRequest>| {
                let posted = posted.clone(); let root = root.clone();
                async move {
                    let disk = read(&root.join("dev-server-external.json")).unwrap();
                    assert!(disk.intents.values().any(|intent| intent.request.operation_id == req.operation_id), "intent must exist before POST");
                    let mut wire = posted.lock().unwrap();
                    wire.posts.push(req.clone());
                    if fail_first && wire.posts.len() == 1 {
                        wire.committed = accept_before_failure;
                        return (axum::http::StatusCode::OK, "truncated response".to_string());
                    }
                    wire.committed = true;
                    (axum::http::StatusCode::ACCEPTED, serde_json::json!({"data": shared_types::RuntimeOperationAccepted {
                        operation_id: req.operation_id.clone(), state: RuntimeOperationState::Accepted,
                        poll: format!("/v1/runtime/operations/{}", req.operation_id),
                    }}).to_string())
                }
            }))
            .route("/v1/runtime/operations/{id}", axum::routing::get(move |axum::extract::Path(id): axum::extract::Path<String>| {
                let queried = queried.clone();
                async move {
                    let wire = queried.lock().unwrap();
                    if !wire.committed { return (axum::http::StatusCode::NOT_FOUND, axum::Json(serde_json::json!({}))); }
                    (axum::http::StatusCode::OK, axum::Json(serde_json::json!({"data": RuntimeOperationView {
                        operation_id: id, kind: RuntimeOperationKind::Restart, state: RuntimeOperationState::Succeeded,
                        request_digest: "digest".into(), revision: 8, runtime_instance_id: "instance".into(),
                        error_code: None, error_message: None, failure_detail: None,
                    }})))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (address, wire, task)
    }

    #[tokio::test]
    async fn lost_reply_then_restart_queries_original_or_replays_identical_request() {
        for accepted in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (address, wire, server) = mock(dir.path().to_path_buf(), true, accepted).await;
            let external = owner(&address);
            let client = OwnerClient::new(&address, &external.token).unwrap();
            let first = manager(dir.path());
            let original = request();
            let intent = first
                .prepare_external_intent("project", dir.path(), &external, &original)
                .unwrap();
            assert!(
                first
                    .resume_external_intent("project", &client, &intent, &original)
                    .await
                    .is_err()
            );
            drop(first);
            let restarted = manager(dir.path());
            let mut later = original.clone();
            later.operation_id = "must-not-be-used".into();
            later.expected_revision = 99;
            let recovered = restarted
                .prepare_external_intent("project", dir.path(), &external, &later)
                .unwrap();
            let view = restarted
                .resume_external_intent("project", &client, &recovered, &later)
                .await
                .unwrap();
            assert_eq!(view.operation_id, original.operation_id);
            let posts = &wire.lock().unwrap().posts;
            assert_eq!(posts.len(), if accepted { 1 } else { 2 });
            for post in posts {
                assert_eq!(
                    serde_json::to_value(post).unwrap(),
                    serde_json::to_value(&original).unwrap()
                );
            }
            restarted
                .finish_external_intent("project", &recovered.request, false)
                .unwrap();
            assert!(restarted.read_external_state().unwrap().intents.is_empty());
            server.abort();
        }
    }

    #[tokio::test]
    async fn private_configuration_is_not_persisted_and_replay_requires_original() {
        let dir = tempfile::tempdir().unwrap();
        let (address, wire, server) = mock(dir.path().to_path_buf(), false, false).await;
        let external = owner(&address);
        let mut original = request();
        original.run_config = Some(shared_types::OperationRunConfig {
            pg: Some(shared_types::StartPgCredential {
                username: "private-runtime-user".into(),
                password: "private-runtime-password".into(),
            }),
        });
        let first = manager(dir.path());
        first
            .prepare_external_intent("project", dir.path(), &external, &original)
            .unwrap();
        let disk = std::fs::read_to_string(first.external_state_path()).unwrap();
        assert!(!disk.contains("private-runtime"));
        assert!(!disk.contains(&external.token));
        drop(first);
        let restarted = manager(dir.path());
        let mut missing = original.clone();
        missing.run_config = None;
        let intent = restarted
            .prepare_external_intent("project", dir.path(), &external, &missing)
            .unwrap();
        let client = OwnerClient::new(&address, &external.token).unwrap();
        assert!(
            restarted
                .resume_external_intent("project", &client, &intent, &missing)
                .await
                .is_err()
        );
        let mut wrong = original.clone();
        wrong
            .run_config
            .as_mut()
            .unwrap()
            .pg
            .as_mut()
            .unwrap()
            .password = "wrong".into();
        assert!(
            restarted
                .resume_external_intent("project", &client, &intent, &wrong)
                .await
                .is_err()
        );
        assert_eq!(wire.lock().unwrap().posts.len(), 0);
        restarted
            .resume_external_intent("project", &client, &intent, &original)
            .await
            .unwrap();
        assert_eq!(wire.lock().unwrap().posts.len(), 1);
        assert!(
            restarted
                .resume_external_intent("project", &client, &intent, &wrong)
                .await
                .is_err(),
            "different newly supplied config must be rejected even after owner committed"
        );
        server.abort();
    }

    #[tokio::test]
    async fn only_verified_preadmission_rejection_releases_local_intent() {
        for (code, cleared) in [
            (shared_types::ERR_REVISION_MISMATCH, true),
            (shared_types::ERR_OPERATION_IN_PROGRESS, true),
            (shared_types::ERR_OPERATION_ID_CONFLICT, false),
            (shared_types::ERR_RECOVERY_REQUIRED, false),
            ("ERR_BACKEND_ERROR", false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let router = axum::Router::new()
                .route(
                    "/v1/runtime/operations/{id}",
                    axum::routing::get(|| async { axum::http::StatusCode::NOT_FOUND }),
                )
                .route(
                    "/v1/runtime/operations",
                    axum::routing::post(move || async move {
                        (
                            axum::http::StatusCode::CONFLICT,
                            axum::Json(serde_json::json!({"code": code, "message": "rejected"})),
                        )
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let manager = manager(dir.path());
            let external = owner(&address);
            let request = request();
            let intent = manager
                .prepare_external_intent("project", dir.path(), &external, &request)
                .unwrap();
            let client = OwnerClient::new(&address, &external.token).unwrap();
            assert!(
                manager
                    .resume_external_intent("project", &client, &intent, &request)
                    .await
                    .is_err()
            );
            let state = manager.read_external_state().unwrap();
            assert_eq!(state.intents.is_empty(), cleared, "{code}");
            assert!(state.owners.contains_key("project"));
            server.abort();
        }
    }

    #[test]
    fn corrupt_unwritable_and_contended_state_prevent_intent_creation() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        std::fs::write(manager.external_state_path(), "broken").unwrap();
        assert!(
            manager
                .prepare_external_intent("project", dir.path(), &owner("127.0.0.1:1"), &request())
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(manager.external_state_path()).unwrap(),
            "broken"
        );
        std::fs::remove_file(manager.external_state_path()).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(manager.external_state_path().with_extension("lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(
            manager
                .prepare_external_intent("project", dir.path(), &owner("127.0.0.1:1"), &request())
                .is_err()
        );
        drop(lock);
        std::fs::create_dir(manager.external_state_path()).unwrap();
        assert!(
            manager
                .prepare_external_intent("project", dir.path(), &owner("127.0.0.1:1"), &request())
                .is_err()
        );
    }

    #[test]
    fn separate_managers_merge_disk_state_and_stale_completion_cannot_clear_intent() {
        let dir = tempfile::tempdir().unwrap();
        let a = manager(dir.path());
        let b = manager(dir.path());
        let original = request();
        let external = owner("127.0.0.1:1");
        a.prepare_external_intent("a", dir.path(), &external, &original)
            .unwrap();
        b.prepare_external_intent("b", dir.path(), &external, &original)
            .unwrap();
        assert_eq!(a.read_external_state().unwrap().intents.len(), 2);
        let mut stale = original.clone();
        stale.operation_id = "stale".into();
        assert!(a.finish_external_intent("a", &stale, false).is_err());
        let mut replacement = external.clone();
        replacement.runtime_instance_id = "new-owner".into();
        let mut replaced_request = original.clone();
        replaced_request.expected_runtime_instance_id = "new-owner".into();
        assert!(
            a.prepare_external_intent("a", dir.path(), &replacement, &replaced_request)
                .is_err()
        );
        assert_eq!(a.read_external_state().unwrap().intents.len(), 2);
    }
}
