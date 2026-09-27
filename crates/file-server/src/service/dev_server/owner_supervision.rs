//! Local management recovery does not replay a business Stop against a new
//! runtime identity. The separate supervisor stops its captured generation and
//! boots management with a durable Stopped instruction.
use super::{DevServerManager, StoppedDev};
use anyhow::{Context, Result, ensure};
use runtime_supervisor::{Action, Request};
use std::{path::Path, time::Duration};

impl DevServerManager {
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
        let (before, offline) =
            match runtime_supervisor::control(&root, Request::new(Action::Status)).await {
                Ok(before) => (before, None),
                Err(error) => {
                    let owner =
                        runtime_supervisor::Owner::try_acquire(&root)?.with_context(|| {
                            format!("independent supervisor unavailable: {error:#}")
                        })?;
                    (runtime_supervisor::last_snapshot(&root)?, Some(owner))
                }
            };
        ensure!(
            before.binding.component == "app-cli" && before.binding.resource == origin,
            "supervisor belongs to another project"
        );
        let generation = before
            .generation
            .clone()
            .context("owner has no execution generation")?;
        let mut request = Request::new(Action::StopWork);
        request.request_id = format!("file-server-stop-{generation}");
        request.expected_generation = Some(generation.clone());
        if let Some(owner) = offline {
            let stopped = owner.stop_offline(&request).await?;
            ensure!(
                stopped.phase == "stopped" && stopped.intent == runtime_supervisor::Intent::Stopped,
                "offline stop is not complete"
            );
            runtime_supervisor::verify_quiescent(&root, &generation)?;
            return Ok(Some(StoppedDev {
                owner_stopped: true,
                killed_pids: Vec::new(),
            }));
        }
        // No queue. A different active explicit control returns Busy immediately.
        runtime_supervisor::control(&root, request.clone()).await?;
        let completed = tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                // Replay this operation, not the latest status of an unrelated
                // subsequent Start. Retain the same identity if the caller retries.
                let snapshot = match runtime_supervisor::control(&root, request.clone()).await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        let snapshot = runtime_supervisor::last_snapshot(&root)?;
                        // One-shot `run` exits after Stop. A cached observation
                        // is usable only for this exact request plus cleanup proof.
                        ensure!(
                            snapshot.supervisor_id == before.supervisor_id
                                && snapshot.operation_id.as_deref() == Some(&request.request_id)
                                && snapshot.phase == "stopped"
                                && snapshot.intent == runtime_supervisor::Intent::Stopped,
                            "stop response unavailable and no matching completion: {error:#}"
                        );
                        snapshot
                    }
                };
                if (snapshot.phase == "ready"
                    && snapshot.generation.as_deref() != Some(&generation))
                    || (snapshot.phase == "stopped"
                        && snapshot.intent == runtime_supervisor::Intent::Stopped
                        && snapshot.operation_id.as_deref() == Some(&request.request_id))
                {
                    runtime_supervisor::verify_quiescent(&root, &generation)?;
                    return Ok::<_, anyhow::Error>(snapshot);
                }
                if snapshot.phase == "recovery_required" {
                    anyhow::bail!(
                        "supervisor recovery required (operation {}): {}",
                        request.request_id,
                        snapshot.error.as_deref().unwrap_or("inspect owner status")
                    );
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await
        .with_context(|| {
            format!(
                "stop {} remains pending; query app-cli owner status",
                request.request_id
            )
        })??;
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
