//! Local management recovery does not replay a business Stop against a new
//! runtime identity. The separate supervisor stops its captured generation and
//! boots management with a durable Stopped instruction.
use super::{DevServerManager, StoppedDev};
use anyhow::Result;
use std::{path::Path, time::Duration};

impl DevServerManager {
    pub(super) fn owner_cleanup_command(
        &self,
        workspace: &Path,
    ) -> runtime_supervisor::CleanupCommand {
        runtime_supervisor::CleanupCommand {
            program: self
                .config
                .app_cli_bin
                .as_deref()
                .unwrap_or("app-cli")
                .into(),
            args: vec!["--app-cli-cleanup-engine".into()],
            cwd: workspace.to_path_buf(),
        }
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
        let completed = runtime_supervisor::stop_work_with_cleanup(
            &root,
            &runtime_supervisor::Binding {
                component: "app-cli".into(),
                resource: origin,
            },
            Duration::from_secs(90),
            &self.owner_cleanup_command(workspace),
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
