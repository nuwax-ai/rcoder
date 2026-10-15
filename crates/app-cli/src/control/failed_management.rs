//! Rebuild an authenticated failed manager before submitting new business work.
use anyhow::{Context as _, Result, ensure};
use runtime_supervisor::{Action, Request, Snapshot};
use shared_types::RuntimeIdentityView;
use std::path::Path;
use tokio::time::Instant;

pub(super) async fn retire_failed_kernel(
    state_root: &Path,
    workspace: &Path,
    addresses: &[String],
    deadline: Instant,
) -> Result<bool> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .context("create failed manager probe")?;
    let mut failed = false;
    for address in addresses {
        let response = tokio::time::timeout_at(
            deadline,
            client
                .get(format!("http://{address}/v1/runtime/identity"))
                .send(),
        )
        .await;
        let Ok(Ok(response)) = response else { continue };
        if response.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE {
            continue;
        }
        let body = tokio::time::timeout_at(deadline, response.json::<serde_json::Value>()).await;
        if let Ok(Ok(body)) = body {
            failed |= body.get("code").and_then(serde_json::Value::as_str)
                == Some("ERR_RECOVERY_REQUIRED");
        }
    }
    if !failed {
        return Ok(false);
    }
    retire_owned_manager(state_root, workspace, deadline).await
}

pub(super) async fn retire_owned_manager(
    state_root: &Path,
    workspace: &Path,
    deadline: Instant,
) -> Result<bool> {
    let original = tokio::time::timeout_at(
        deadline,
        runtime_supervisor::control(state_root, Request::new(Action::Status)),
    )
    .await
    .context("capture failed manager deadline")??;
    retire_captured_manager(state_root, workspace, &original, deadline).await
}

/// A failed credential probe is about one observed HTTP instance. Capture its
/// native authority before yielding again, then recheck both surfaces before
/// issuing Shutdown. A successor is rediscovered instead of being retired.
pub(super) async fn retire_observed_manager(
    state_root: &Path,
    workspace: &Path,
    address: &str,
    observed: &RuntimeIdentityView,
    deadline: Instant,
) -> Result<bool> {
    tokio::time::timeout_at(deadline, async {
        let original = runtime_supervisor::control(state_root, Request::new(Action::Status))
            .await
            .context("capture credential-failed manager authority")?;
        verify_workspace(state_root, workspace, &original)?;
        if super::owner_dispatch::verify_saved_management_identity(state_root, observed).is_err() {
            return Ok(false);
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .context("create credential-failed manager identity probe")?;
        let response = client
            .get(format!("http://{address}/v1/runtime/identity"))
            .send()
            .await
            .context("recheck credential-failed manager HTTP identity")?;
        if !response.status().is_success() {
            return Ok(false);
        }
        let body: serde_json::Value = response
            .json()
            .await
            .context("decode credential-failed manager identity envelope")?;
        let current: RuntimeIdentityView = serde_json::from_value(
            body.get("data")
                .cloned()
                .context("credential-failed manager identity is missing")?,
        )
        .context("decode credential-failed manager HTTP identity")?;
        if !super::owner_dispatch::same_management_identity(observed, &current)
            || super::owner_dispatch::verify_saved_management_identity(state_root, observed)
                .is_err()
        {
            return Ok(false);
        }
        let after = match runtime_supervisor::control_verified(
            state_root,
            Request::new(Action::Status),
            &original.supervisor_id,
        )
        .await
        {
            Ok(after) => after,
            Err(error) if identity_changed(&error) => return Ok(false),
            Err(error) => return Err(error).context("recheck credential-failed native authority"),
        };
        if after.supervisor_id != original.supervisor_id
            || after.binding != original.binding
            || after.generation != original.generation
            || after.intent != original.intent
            || after.operation_id != original.operation_id
        {
            return Ok(false);
        }
        // Do not recapture a fresher owner here. The conditional Shutdown and
        // its original request ID belong to this exact native observation.
        retire_captured_manager(state_root, workspace, &original, deadline).await
    })
    .await
    .context("credential-failed manager retirement deadline")?
}

fn verify_workspace(state_root: &Path, workspace: &Path, original: &Snapshot) -> Result<()> {
    let expected = runtime_state_layout::resolve_project_origin(workspace)?;
    let managed = runtime_state_layout::ManagedWorkspace::from_env(workspace, state_root)?;
    let actual = match &managed {
        Some(managed) => managed.verify_contained_workspace(&original.binding.resource)?,
        None => runtime_state_layout::resolve_project_origin(&original.binding.resource)?,
    };
    ensure!(
        original.binding.component == "app-cli"
            && (managed.is_some()
                || runtime_state_layout::canonical_project_root(&actual)
                    == runtime_state_layout::canonical_project_root(&expected)),
        "failed manager belongs to another application workspace"
    );
    Ok(())
}

async fn retire_captured_manager(
    state_root: &Path,
    workspace: &Path,
    original: &Snapshot,
    deadline: Instant,
) -> Result<bool> {
    verify_workspace(state_root, workspace, original)?;
    let (captured, shutdown) = super::managed_owner::upgrade_shutdown_request(state_root, original)
        .context("resume original captured manager Shutdown")?;
    let result = tokio::time::timeout_at(
        deadline,
        runtime_supervisor::shutdown_captured_owner(
            state_root,
            &captured,
            &shutdown,
            deadline.saturating_duration_since(Instant::now()),
        ),
    )
    .await
    .context("retire failed manager deadline")?;
    if let Err(error) = result {
        if identity_changed(&error) {
            // The exact conditional Shutdown cannot affect a new generation.
            // No business request has been sent; rediscover that live owner.
            return Ok(false);
        }
        return Err(error).context("retire original failed management owner");
    }
    tracing::warn!(supervisor = %original.supervisor_id,
        "failed management kernel retired; normal bootstrap can rebuild it");
    Ok(true)
}

fn identity_changed(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<runtime_supervisor::Problem>()
        .is_some_and(|problem| problem.code == runtime_supervisor::FailureCode::IdentityChanged)
}

#[cfg(test)]
#[path = "failed_management_tests.rs"]
mod tests;
