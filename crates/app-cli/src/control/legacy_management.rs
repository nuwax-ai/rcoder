//! Retire live, same-application legacy owners before binding the new manager.
//! Offline history stays under RuntimeStore's sealed legacy owner leases.
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use runtime_supervisor::{Action, Owner, Request};
use tokio::time::Instant;

pub(crate) async fn prepare_legacy_management(
    workspace: &Path,
    state_root: &Path,
    deadline: Instant,
) -> Result<()> {
    let authorities =
        crate::runtime_kernel::RuntimeStore::discover_legacy_authorities(state_root, workspace)?;
    let expected_project = runtime_state_layout::resolve_project_origin(workspace)?;
    let managed = runtime_state_layout::ManagedWorkspace::from_env(workspace, state_root)?;
    for (legacy_root, selected_binding) in authorities {
        ensure!(
            Instant::now() < deadline,
            "legacy management recovery deadline exceeded"
        );
        if let Some(lease) = Owner::try_acquire(&legacy_root)? {
            // Acquirable OS authority permits the store to seal and reconcile
            // this root. Metadata/PID fields cannot turn it into a live owner.
            drop(lease);
            continue;
        }
        let status = tokio::time::timeout_at(
            deadline,
            runtime_supervisor::control(&legacy_root, Request::new(Action::Status)),
        )
        .await
        .context("capture live legacy management deadline")?;
        let original = match status {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if let Some(lease) = Owner::try_acquire(&legacy_root)? {
                    drop(lease);
                    continue;
                }
                return Err(error).with_context(|| format!(
                    "legacy owner at {} still holds its execution lock but authenticated native management is unavailable; no process was killed",
                    legacy_root.display(),
                ));
            }
        };
        let original_project = match &managed {
            Some(managed) => managed.verify_contained_workspace(&original.binding.resource)?,
            None => runtime_state_layout::resolve_project_origin(&original.binding.resource)?,
        };
        ensure!(
            selected_binding.component == "app-cli"
                && original.binding.component == "app-cli"
                && (managed.is_some()
                    || runtime_state_layout::canonical_project_root(&original_project)
                        == runtime_state_layout::canonical_project_root(&expected_project)),
            "authenticated legacy management belongs to another application or project"
        );
        let (captured, shutdown) =
            super::managed_owner::upgrade_shutdown_request(&legacy_root, &original)
                .context("select original live legacy Shutdown identity")?;
        tokio::time::timeout_at(
            deadline,
            runtime_supervisor::shutdown_captured_owner(
                &legacy_root,
                &captured,
                &shutdown,
                deadline.saturating_duration_since(Instant::now()),
            ),
        )
        .await
        .context("live legacy management cleanup deadline")?
        .with_context(|| {
            format!(
                "retire captured legacy supervisor {} using original Shutdown {}",
                captured.supervisor_id, shutdown.request_id,
            )
        })?;
        tracing::warn!(
            supervisor = %captured.supervisor_id,
            operation_id = %shutdown.request_id,
            root = %legacy_root.display(),
            "same-application legacy management retired before current Source bootstrap"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn held_legacy_lock_without_native_channel_is_reported_without_erasing_authority() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let legacy = project.join(runtime_state_layout::STATE_DIR_NAME);
        let original = Owner::try_acquire(&legacy).unwrap().unwrap();
        let identity = serde_json::to_vec(&serde_json::json!({
            "service_family": "userapp-dev", "source_root": project,
        }))
        .unwrap();
        std::fs::write(legacy.join("identity.json"), &identity).unwrap();
        let current = directory.path().join("current-root");
        let error = prepare_legacy_management(
            &project,
            &current,
            Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("authenticated native management is unavailable"));
        assert!(
            Owner::try_acquire(&legacy).unwrap().is_none(),
            "the real original lock must remain held"
        );
        assert_eq!(
            std::fs::read(legacy.join("identity.json")).unwrap(),
            identity
        );
        assert!(legacy.join("owner.lock").is_file());
        drop(original);
    }
}
