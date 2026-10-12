//! Observation of the original Stop after its HTTP wait budget has expired.
//! No owner bootstrap, new Stop identity, or write replay is performed here.
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, ensure};
use shared_types::RuntimeOperationKind;

use super::{DevServerManager, external_store, owner_client};
use crate::error::{AppError, AppResult};

/// In-memory references to the Stop this process has just attempted. These are
/// activity observations, not an extra persisted receipt or admission fence.
#[derive(Clone)]
pub enum PendingStopActivity {
    External {
        project: String,
        intent: Box<ExternalStopObservation>,
    },
    Local(runtime_supervisor::StopWorkAttempt),
    LegacyExternal(Box<LegacyStopObservation>),
}

#[derive(Clone)]
pub struct LegacyStopObservation {
    application_id: String,
    operation_id: String,
    runtime_instance_id: String,
    workspace_id: String,
    address: String,
    workspace: PathBuf,
    state_root: Option<PathBuf>,
}

#[derive(Clone)]
pub struct ExternalStopObservation {
    request: shared_types::RuntimeOperationRequest,
    address: String,
    workspace: PathBuf,
    state_root: Option<PathBuf>,
}

struct CapturedActivity {
    observation: PendingStopActivity,
    pending: bool,
}
tokio::task_local! {
    static STOP_ACTIVITY: Arc<Mutex<Vec<CapturedActivity>>>;
}

fn update(
    activity: impl FnOnce(&mut Vec<CapturedActivity>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    match STOP_ACTIVITY.try_with(|record| {
        let mut record = record
            .lock()
            .map_err(|_| anyhow::anyhow!("Stop activity capture lock poisoned"))?;
        activity(&mut record)
    }) {
        Ok(result) => result,
        Err(_) => Ok(()), // Other owner operations and untracked callers are unchanged.
    }
}

pub(super) fn prepare_external(
    project: &str,
    request: &shared_types::RuntimeOperationRequest,
    address: &str,
    workspace: &Path,
    state_root: Option<PathBuf>,
) -> anyhow::Result<()> {
    if request.kind != RuntimeOperationKind::Stop {
        return Ok(());
    }
    update(|record| {
        record.push(CapturedActivity {
            pending: false,
            observation: PendingStopActivity::External {
                project: project.into(),
                intent: Box::new(ExternalStopObservation {
                    request: request.clone(),
                    address: address.into(),
                    workspace: workspace.into(),
                    state_root,
                }),
            },
        });
        Ok(())
    })
}

pub(super) fn external_dispatched(
    request: &shared_types::RuntimeOperationRequest,
) -> anyhow::Result<()> {
    if request.kind != RuntimeOperationKind::Stop {
        return Ok(());
    }
    update(|record| {
        let captured = record.iter_mut().find(|captured| matches!(&captured.observation,
            PendingStopActivity::External { intent, .. } if intent.request.operation_id == request.operation_id
                && intent.request.expected_runtime_instance_id == request.expected_runtime_instance_id))
            .context("Stop dispatch has no captured original target")?;
        captured.pending = true;
        Ok(())
    })
}

pub(super) fn external_observed(view: &shared_types::RuntimeOperationView) -> anyhow::Result<()> {
    if view.kind != RuntimeOperationKind::Stop {
        return Ok(());
    }
    update(|record| {
        for captured in record {
            if let PendingStopActivity::External { intent, .. } = &captured.observation
                && intent.request.operation_id == view.operation_id
            {
                external_store::verify_view(view, &intent.request)?;
                captured.pending = !view.state.is_terminal();
            }
            if let PendingStopActivity::LegacyExternal(original) = &captured.observation
                && original.operation_id == view.operation_id
            {
                ensure!(
                    original.runtime_instance_id == view.runtime_instance_id,
                    "legacy Stop observation changed owner"
                );
                captured.pending = !view.state.is_terminal();
            }
        }
        Ok(())
    })
}

pub(super) fn prepare_legacy(
    operation_id: &str,
    identity: &shared_types::RuntimeIdentityView,
    address: &str,
) -> anyhow::Result<()> {
    shared_types::validate_identifier(operation_id, "operation_id").map_err(anyhow::Error::msg)?;
    update(|record| {
        record.push(CapturedActivity {
            pending: false,
            observation: PendingStopActivity::LegacyExternal(Box::new(LegacyStopObservation {
                application_id: identity.application_id.clone(),
                operation_id: operation_id.into(),
                runtime_instance_id: identity.runtime_instance_id.clone(),
                workspace_id: identity.workspace_id.clone(),
                address: address.into(),
                workspace: PathBuf::from(&identity.source_root),
                state_root: owner_client::find_owner_token(
                    Path::new(&identity.source_root),
                    &identity.application_id,
                )
                .map(|(root, _token)| root),
            })),
        });
        Ok(())
    })
}

fn legacy_durable_outcome(
    original: &LegacyStopObservation,
) -> anyhow::Result<Option<shared_types::RuntimeOperationView>> {
    let Some(root) = &original.state_root else {
        return Ok(None);
    };
    let bytes = match std::fs::read(
        root.join("operations")
            .join(format!("{}.json", original.operation_id)),
    ) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read original legacy Stop outcome"),
    };
    let record: serde_json::Value = serde_json::from_slice(&bytes)?;
    let request: shared_types::RuntimeOperationRequest = serde_json::from_value(
        record
            .get("request")
            .cloned()
            .context("legacy Stop durable request missing")?,
    )?;
    ensure!(
        request.operation_id == original.operation_id
            && request.expected_runtime_instance_id == original.runtime_instance_id
            && request.workspace_id == original.workspace_id
            && request.kind == RuntimeOperationKind::Stop,
        "legacy Stop durable request identity changed"
    );
    // The digest/revision come from the original request bytes, not an
    // invented revision reconstructed from the old transport cache.
    owner_client::read_durable_stop_outcome(root, &request)
}

pub(super) fn external_not_admitted(operation_id: &str) -> anyhow::Result<()> {
    update(|record| {
        for captured in record {
            if matches!(&captured.observation, PendingStopActivity::External { intent, .. }
                if intent.request.operation_id == operation_id)
            {
                captured.pending = false;
            }
        }
        Ok(())
    })
}

pub(super) fn native_dispatched(
    attempt: &runtime_supervisor::StopWorkAttempt,
) -> anyhow::Result<()> {
    if attempt.mode == runtime_supervisor::StopMode::Unresolved {
        return Ok(());
    }
    update(|record| {
        if let Some(captured) = record.iter_mut().find(|captured| matches!(&captured.observation,
            PendingStopActivity::Local(original) if original.request.request_id == attempt.request.request_id)) {
            captured.observation = PendingStopActivity::Local(attempt.clone());
            captured.pending = true;
        } else {
            record.push(CapturedActivity { observation: PendingStopActivity::Local(attempt.clone()), pending: true });
        }
        Ok(())
    })
}

pub(super) fn native_finished(request_id: &str) -> anyhow::Result<()> {
    update(|record| {
        for captured in record {
            if matches!(&captured.observation, PendingStopActivity::Local(attempt)
                if attempt.request.request_id == request_id)
            {
                captured.pending = false;
            }
        }
        Ok(())
    })
}

impl DevServerManager {
    /// Capture actual writes/observed execution inside the original Stop future.
    /// A metadata read error before dispatch cannot create activity; a later
    /// cache error cannot erase an in-memory reference to an already sent Stop.
    pub async fn stop_userapp_dev_tracked(
        &self,
        project: &str,
        workspace: &Path,
    ) -> (AppResult<super::StoppedDev>, Vec<PendingStopActivity>) {
        let record = Arc::new(Mutex::new(Vec::new()));
        let result = STOP_ACTIVITY
            .scope(record.clone(), self.stop_userapp_dev(project, workspace))
            .await;
        // All updates are short internal operations; no caller code executes
        // under this mutex. Retain captured references even if it is poisoned.
        let mut captured = match record.lock() {
            Ok(captured) => captured,
            Err(poisoned) => poisoned.into_inner(),
        };
        let pending = captured
            .drain(..)
            .filter(|captured| captured.pending)
            .map(|captured| captured.observation)
            .collect();
        (result, pending)
    }

    /// `true` means the captured execution has actually settled. It does not
    /// turn the original failed HTTP response into success or stop new work.
    pub async fn observe_stop_activity(
        &self,
        observation: &PendingStopActivity,
    ) -> AppResult<bool> {
        let observe = async {
            match observation {
                PendingStopActivity::LegacyExternal(original) => {
                    if let Some(view) = legacy_durable_outcome(original)?
                        && view.state.is_terminal()
                    {
                        return Ok(true);
                    }
                    let Some(identity) = owner_client::probe_owner(&original.address).await? else {
                        return Ok(false);
                    };
                    owner_client::verify_stop_identity(
                        &identity,
                        &original.workspace,
                        &original.application_id,
                    )?;
                    ensure!(
                        identity.workspace_id == original.workspace_id,
                        "legacy Stop workspace changed"
                    );
                    let (_, token) = owner_client::find_owner_token(
                        Path::new(&identity.source_root),
                        &identity.application_id,
                    )
                    .context("legacy Stop observation credentials unavailable")?;
                    let client = owner_client::OwnerClient::new(&original.address, &token)?;
                    let Some(view) = client.operation_if_exists(&original.operation_id).await?
                    else {
                        return Ok(false);
                    };
                    ensure!(
                        view.operation_id == original.operation_id
                            && view.runtime_instance_id == original.runtime_instance_id
                            && view.kind == RuntimeOperationKind::Stop,
                        "legacy Stop outcome changed identity"
                    );
                    Ok(view.state.is_terminal())
                }
                PendingStopActivity::External { project, intent } => {
                    // A captured owner can commit the original outcome and
                    // then exit before the next poll. Use the same strict
                    // durable-result verification as the synchronous Stop.
                    if let Some(root) = &intent.state_root
                        && let Some(view) =
                            owner_client::read_durable_stop_outcome(root, &intent.request)?
                        && view.state.is_terminal()
                    {
                        return Ok(true);
                    }
                    let Some(identity) = owner_client::probe_owner(&intent.address).await? else {
                        return Ok(false);
                    };
                    let app_id = project.strip_prefix("userapp:").unwrap_or(project);
                    owner_client::verify_stop_identity(&identity, &intent.workspace, app_id)?;
                    let (root, token) = owner_client::find_owner_token(
                        Path::new(&identity.source_root),
                        &identity.application_id,
                    )
                    .context("original Stop observation credentials unavailable")?;
                    let client = owner_client::OwnerClient::new(&intent.address, &token)?;
                    let view = match client
                        .operation_if_exists(&intent.request.operation_id)
                        .await?
                    {
                        Some(view) => Some(view),
                        None => owner_client::read_durable_stop_outcome(&root, &intent.request)?,
                    };
                    let Some(view) = view else {
                        return Ok(false);
                    };
                    external_store::verify_view(&view, &intent.request)?;
                    if !view.state.is_terminal() {
                        return Ok(false);
                    }
                    Ok(true)
                }
                PendingStopActivity::Local(attempt) => {
                    let supervisor = match &attempt.mode {
                        runtime_supervisor::StopMode::Online { supervisor_id }
                        | runtime_supervisor::StopMode::OfflineChosen { supervisor_id } => {
                            supervisor_id
                        }
                        runtime_supervisor::StopMode::Unresolved => return Ok(true), // No write was dispatched.
                    };
                    if let Some(generation) = &attempt.captured_generation
                        && runtime_supervisor::verify_local_quiescent_for_supervisor(
                            &attempt.root,
                            generation,
                            supervisor,
                        )?
                        .is_some()
                    {
                        return Ok(true);
                    }
                    let Some(receipt) = runtime_supervisor::saved_request_snapshot(
                        &attempt.root,
                        &attempt.request,
                    )?
                    else {
                        return Ok(false);
                    };
                    ensure!(
                        receipt.binding == attempt.binding && receipt.supervisor_id == *supervisor,
                        "original native Stop observation changed owner"
                    );
                    Ok(
                        receipt.operation_id.as_deref()
                            == Some(attempt.request.request_id.as_str())
                            && receipt.phase == runtime_supervisor::Phase::Stopped
                            && receipt.intent == runtime_supervisor::Intent::Stopped,
                    )
                }
            }
        }
        .await;
        observe.map_err(|error| AppError::owner_error("observe original Stop activity", error))
    }
}

#[cfg(test)]
#[path = "stop_activity_submit_tests.rs"]
mod submit_observation_tests;
