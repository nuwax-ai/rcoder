//! Managed application discovery repair. Source files and execution journals
//! stay in place; the platform root and its owner lock remain authoritative.

use std::{io::Write, path::Path};

use anyhow::{Context, Result, ensure};
use runtime_state_layout::ManagedWorkspace;
use runtime_supervisor::Binding;

pub(crate) fn previous_binding(workspace: &Path, state_root: &Path) -> Result<Option<Binding>> {
    let Some(managed) = ManagedWorkspace::from_env(workspace, state_root)? else {
        return Ok(None);
    };
    // Readable foreign identity is not stale metadata for this application.
    let _ = read_identity(&managed, false)?;
    let path = state_root.join("supervisor.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read managed supervisor discovery"),
    };
    previous_binding_from_bytes(&managed, &bytes)
}

fn previous_binding_from_bytes(
    managed: &ManagedWorkspace,
    bytes: &[u8],
) -> Result<Option<Binding>> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        // The generic owner reader preserves and rebuilds corrupt discovery.
        return Ok(None);
    };
    let Some(binding) = value.pointer("/snapshot/binding") else {
        return Ok(None);
    };
    // Inspect individually readable authority fields before treating a damaged
    // structure as reconstructible. A broken second field cannot hide a foreign
    // component or an absolute path outside the managed application.
    if let Some(component) = binding.get("component").and_then(serde_json::Value::as_str)
        && !component.trim().is_empty()
    {
        ensure!(
            component == "app-cli",
            "managed state belongs to another component"
        );
    }
    if let Some(resource) = binding.get("resource").and_then(serde_json::Value::as_str)
        && Path::new(resource).is_absolute()
    {
        managed.verify_contained_workspace(Path::new(resource))?;
    }
    let previous: Binding = match serde_json::from_value(binding.clone()) {
        Ok(previous) => previous,
        Err(error) => {
            tracing::warn!(%error, "damaged managed supervisor binding will be preserved by native recovery");
            return Ok(None);
        }
    };
    ensure!(
        previous.component == "app-cli",
        "managed state belongs to another component"
    );
    managed.verify_contained_workspace(&previous.resource)?;
    Ok((previous.resource != managed.source_root).then_some(previous))
}

/// Keep a verified logical key stable while source_root and runtime instance
/// follow the current owner. Bad discovery is archived, not a permanent hold.
pub(crate) fn workspace_id(
    workspace: &Path,
    state_root: &Path,
    default_id: String,
) -> Result<String> {
    let Some(managed) = ManagedWorkspace::from_env(workspace, state_root)? else {
        return Ok(default_id);
    };
    let Some((identity, bytes)) = read_identity(&managed, true)? else {
        return Ok(default_id);
    };
    if shared_types::validate_identifier(&identity.workspace_id, "workspace_id").is_err() {
        preserve_identity(&managed, &bytes)?;
        tracing::warn!(
            "invalid managed workspace key preserved; rebuilding discovery from platform identity"
        );
        return Ok(default_id);
    }
    if Path::new(&identity.source_root) != managed.source_root {
        preserve_identity(&managed, &bytes)?;
    }
    Ok(identity.workspace_id)
}

fn read_identity(
    managed: &ManagedWorkspace,
    preserve_invalid: bool,
) -> Result<Option<(shared_types::RuntimeIdentityView, Vec<u8>)>> {
    let bytes = match std::fs::read(managed.state_root.join("identity.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read managed owner identity"),
    };
    if let Ok(raw) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        if let Some(app) = raw
            .get("application_id")
            .and_then(serde_json::Value::as_str)
            .filter(|app| !app.trim().is_empty())
        {
            ensure!(
                app == managed.application_id,
                "owner identity belongs to another application"
            );
        }
        if let Some(family) = raw
            .get("service_family")
            .and_then(serde_json::Value::as_str)
            .filter(|family| !family.trim().is_empty())
        {
            ensure!(
                family == "userapp-dev",
                "owner identity belongs to another service family"
            );
        }
        if let Some(root) = raw.get("source_root").and_then(serde_json::Value::as_str)
            && Path::new(root).is_absolute()
        {
            managed.verify_contained_workspace(Path::new(root))?;
        }
    }
    match serde_json::from_slice::<shared_types::RuntimeIdentityView>(&bytes) {
        Ok(identity)
            if identity.application_id == managed.application_id
                && identity.service_family == "userapp-dev"
                && Path::new(&identity.source_root).is_absolute() =>
        {
            Ok(Some((identity, bytes)))
        }
        Ok(_) => {
            if preserve_invalid {
                preserve_identity(managed, &bytes)?;
            }
            tracing::warn!(
                "incomplete managed identity preserved; rebuilding from platform context"
            );
            Ok(None)
        }
        Err(error) => {
            if preserve_invalid {
                preserve_identity(managed, &bytes)?;
            }
            tracing::warn!(%error, "damaged managed owner identity preserved; native execution recovery remains authoritative");
            Ok(None)
        }
    }
}

fn preserve_identity(managed: &ManagedWorkspace, bytes: &[u8]) -> Result<()> {
    let backup = managed.state_root.join(format!(
        "identity.recovered-{}.json",
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = tempfile::NamedTempFile::new_in(&managed.state_root)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(backup)
        .map_err(|error| error.error)
        .context("preserve original managed owner identity")?;
    #[cfg(unix)]
    std::fs::File::open(&managed.state_root)?.sync_all()?;
    Ok(())
}

/// Retire only the verified live owner of this managed application's authority.
/// The caller then re-enters normal bootstrap; it never starts a second owner.
pub(crate) async fn retire_misdirected(
    managed: &ManagedWorkspace,
    identity: &shared_types::RuntimeIdentityView,
) -> Result<()> {
    ensure!(
        identity.application_id == managed.application_id
            && identity.service_family == "userapp-dev"
            && identity.protocol_version == shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
        "owner is not a compatible runtime for this managed application"
    );
    let source = managed.verify_contained_workspace(Path::new(&identity.source_root))?;
    let verify_saved = || -> Result<()> {
        let (saved, _) =
            read_identity(managed, false)?.context("live owner identity record unavailable")?;
        ensure!(
            saved.runtime_instance_id == identity.runtime_instance_id
                && saved.workspace_id == identity.workspace_id
                && saved.source_root == identity.source_root,
            "managed owner changed before shutdown"
        );
        Ok(())
    };
    verify_saved()?;
    let snapshot = runtime_supervisor::control(
        &managed.state_root,
        runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
    )
    .await?;
    ensure!(
        snapshot.binding.component == "app-cli"
            && managed.verify_contained_workspace(&snapshot.binding.resource)? == source,
        "managed native supervisor does not match the observed owner"
    );
    verify_saved()?;
    let mut request = runtime_supervisor::Request::new(runtime_supervisor::Action::Shutdown);
    request.capture_generation(snapshot.generation.as_deref());
    match runtime_supervisor::shutdown_captured_owner(
        &managed.state_root,
        &snapshot,
        &request,
        std::time::Duration::from_secs(45),
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(error)
            if error
                .downcast_ref::<runtime_supervisor::Problem>()
                .is_some_and(|problem| {
                    problem.code == runtime_supervisor::FailureCode::IdentityChanged
                }) =>
        {
            // No business request has been submitted. Bootstrap rechecks the
            // successor and its ownership; a changed instance is never stopped.
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_binding_defers_to_native_recovery_without_hiding_foreign_authority() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().canonicalize().unwrap().join("application");
        let state = source.join("state/application");
        std::fs::create_dir_all(&state).unwrap();
        let managed = ManagedWorkspace {
            application_id: "application".into(),
            source_root: source.clone(),
            state_root: state,
        };
        for binding in [
            json!({"component":"app-cli", "resource":123}),
            json!({"resource":source}),
        ] {
            let bytes = serde_json::to_vec(&json!({"snapshot":{"binding":binding}})).unwrap();
            assert!(
                previous_binding_from_bytes(&managed, &bytes)
                    .unwrap()
                    .is_none()
            );
        }
        let foreign = source.with_file_name("another-application");
        for binding in [
            json!({"component":"foreign-cli", "resource":123}),
            json!({"resource":foreign}),
            json!({"component":"app-cli", "resource":foreign}),
        ] {
            let bytes = serde_json::to_vec(&json!({"snapshot":{"binding":binding}})).unwrap();
            assert!(
                previous_binding_from_bytes(&managed, &bytes).is_err(),
                "readable foreign authority must survive a malformed sibling field"
            );
        }
        let wrong_root = source.join("old-project");
        let bytes = serde_json::to_vec(
            &json!({"snapshot":{"binding":{"component":"app-cli", "resource":wrong_root}}}),
        )
        .unwrap();
        assert_eq!(
            previous_binding_from_bytes(&managed, &bytes)
                .unwrap()
                .unwrap()
                .resource,
            wrong_root
        );
    }
}
