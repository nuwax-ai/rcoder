//! Local management recovery does not replay a business Stop against a new
//! runtime identity. The separate supervisor stops its captured generation and
//! boots management with a durable Stopped instruction.
use super::{DevServerManager, StoppedDev};
use anyhow::{Context, Result};
use std::{path::Path, time::Duration};

impl DevServerManager {
    pub(super) fn owner_cleanup_command(
        &self,
        workspace: &Path,
        state_root: &Path,
    ) -> Result<runtime_supervisor::CleanupCommand> {
        let mut program =
            std::path::PathBuf::from(self.config.app_cli_bin.as_deref().unwrap_or("app-cli"));
        if program.is_relative()
            && program
                .parent()
                .is_some_and(|parent| !parent.as_os_str().is_empty())
        {
            // Keep the original command's relative-path semantics even though
            // cleanup runs from the stable state authority. A bare app-cli name
            // continues to use PATH lookup.
            program = std::path::absolute(workspace)
                .context("resolve cleanup program workspace")?
                .join(program);
        }
        Ok(runtime_supervisor::CleanupCommand {
            program,
            args: vec!["--app-cli-cleanup-engine".into()],
            cwd: state_root.to_path_buf(),
        })
    }

    pub(super) async fn stop_supervised_owner(
        &self,
        project: &str,
        workspace: &Path,
    ) -> Result<Option<StoppedDev>> {
        let origin = runtime_state_layout::resolve_project_origin(workspace)?;
        let Some(root) = runtime_state_layout::resolve_state_root(
            &origin,
            std::env::var_os("APP_CLI_STATE_ROOT").as_deref(),
            std::env::var_os("PROJECT_ID").as_deref(),
        )?
        else {
            return Ok(None);
        };
        if !root.join("supervisor.json").try_exists()? {
            return Ok(None);
        }
        let cleanup = self.owner_cleanup_command(workspace, &root)?;
        let completed = runtime_supervisor::stop_work_with_cleanup(
            &root,
            &runtime_supervisor::Binding {
                component: "app-cli".into(),
                resource: origin,
            },
            Duration::from_secs(90),
            &cleanup,
        )
        .await?;
        // The successor acknowledged only after startup quiescence (including
        // supervisord groups) and writing desired=Stopped. Old uncertain writes
        // remain in the owner's journal. Transport damage cannot undo this proof.
        if let Err(error) = self.recover_owner_if_needed(project, workspace).await {
            tracing::warn!(%project, %error, generation = ?completed.generation,
                "business stopped; transport registration still needs reconciliation");
        }
        Ok(Some(StoppedDev {
            owner_stopped: true,
            killed_pids: Vec::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn cleanup_uses_state_root_and_preserves_original_program_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("removed-workspace");
        let state_root = temp.path().join("state");
        std::fs::create_dir_all(&state_root).unwrap();
        for program in ["app-cli", "./tools/app-cli"] {
            let mut config = crate::Config::from_env().unwrap();
            config.app_cli_bin = Some(program.into());
            let manager = DevServerManager::new(Arc::new(config));
            let command = manager
                .owner_cleanup_command(&workspace, &state_root)
                .unwrap();
            assert_eq!(command.cwd, state_root);
            assert_eq!(
                command.program,
                if program == "app-cli" {
                    std::path::PathBuf::from(program)
                } else {
                    std::path::absolute(&workspace).unwrap().join(program)
                }
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_adapter_runs_after_original_workspace_is_removed() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("old-code");
        let state_root = temp.path().join("state");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&state_root).unwrap();
        std::fs::write(state_root.join("SENTRY"), "preserve").unwrap();
        let binary = temp.path().join("cleanup-fixture.sh");
        std::fs::write(
            &binary,
            "#!/bin/sh\n[ \"$1\" = --app-cli-cleanup-engine ] || exit 12\npwd -P\n",
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir(&workspace).unwrap();
        let mut config = crate::Config::from_env().unwrap();
        config.app_cli_bin = Some(binary.display().to_string());
        let manager = DevServerManager::new(Arc::new(config));
        let cleanup = manager
            .owner_cleanup_command(&workspace, &state_root)
            .unwrap();
        let result = tokio::process::Command::new(&cleanup.program)
            .args(&cleanup.args)
            .current_dir(&cleanup.cwd)
            .output()
            .await
            .unwrap();
        assert!(result.status.success());
        assert_eq!(
            String::from_utf8(result.stdout).unwrap().trim(),
            std::fs::canonicalize(&state_root)
                .unwrap()
                .to_string_lossy()
        );
        assert!(
            !workspace.exists(),
            "cleanup must not recreate the removed project directory"
        );
        assert_eq!(
            std::fs::read_to_string(state_root.join("SENTRY")).unwrap(),
            "preserve"
        );
    }
}
