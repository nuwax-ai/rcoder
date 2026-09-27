//! app-cli adapter for the common local supervisor. Business journals remain
//! owned by the worker; the parent only records process-control intent.
use anyhow::Result;
use runtime_supervisor::{Action, Options, Owner, Request, Worker};
use std::path::{Path, PathBuf};

/// Interactive service shutdown grace, shared by both execution engines.
pub(crate) const STOP_GRACE_SECONDS: u64 = 3;

/// Called by the retained guardian, including after a stuck worker is killed.
pub async fn cleanup_external_engine() -> Result<()> {
    use anyhow::Context;
    runtime_supervisor::verify_cleanup_callback()?;
    let root = std::env::var_os("RCODER_SUPERVISOR_CLEANUP_ROOT")
        .context("cleanup callback root missing")?;
    crate::supervisord_host::SupervisordHost::cleanup_generation(Path::new(&root)).await
}

/// Private runtime adapter, used through an identity-bound container exec.
/// No tokens, environment secrets or migration results are returned.
pub fn physical_domain_recovery(mut args: impl Iterator<Item = std::ffi::OsString>) -> Result<()> {
    use anyhow::Context;
    let action = args.next().context("domain recovery action missing")?;
    let workspace = PathBuf::from(args.next().context("domain recovery workspace missing")?);
    let binding = runtime_supervisor::Binding {
        component: "app-cli".into(),
        resource: runtime_state_layout::resolve_project_origin(&workspace)?,
    };
    let root = scope(&workspace)?;
    match action.to_str() {
        Some("inspect") => {
            anyhow::ensure!(args.next().is_none(), "unexpected inspection argument");
            println!(
                "{}",
                serde_json::to_string(&runtime_supervisor::domain::pending(&root, &binding)?)?
            );
        }
        Some("confirm") => {
            let proof = args.next().context("physical exit evidence missing")?;
            anyhow::ensure!(args.next().is_none(), "unexpected confirmation argument");
            let proof = serde_json::from_str(proof.to_str().context("invalid evidence encoding")?)?;
            runtime_supervisor::domain::publish_confirmed_exit(&root, &binding, &proof)?;
        }
        _ => anyhow::bail!("unsupported physical domain recovery action"),
    }
    Ok(())
}

/// Best-effort bounded preparation for platform container retirement. The
/// caller must still stop the captured container if this cannot finish.
pub async fn drain_for_container_stop(workspace: &Path) -> Result<()> {
    let root = scope(workspace)?;
    if !root.join("supervisor.json").try_exists()? {
        return Ok(());
    }
    let before = runtime_supervisor::control(&root, Request::new(Action::Status)).await?;
    anyhow::ensure!(
        before.binding.component == "app-cli"
            && before.binding.resource == runtime_state_layout::resolve_project_origin(workspace)?,
        "container stop workspace mismatch"
    );
    let mut request = Request::new(Action::Shutdown);
    request.expected_generation = before.generation.clone();
    runtime_supervisor::control(&root, request.clone()).await?;
    tokio::time::timeout(std::time::Duration::from_secs(7), async {
        loop {
            let snapshot = match runtime_supervisor::control(&root, request.clone()).await {
                Ok(snapshot) => snapshot,
                Err(_) => runtime_supervisor::last_snapshot(&root)?,
            };
            anyhow::ensure!(
                snapshot.supervisor_id == before.supervisor_id,
                "container owner changed during drain"
            );
            if snapshot.phase == runtime_supervisor::Phase::Stopped
                && snapshot.operation_id.as_deref() == Some(&request.request_id)
            {
                if let Some(id) = before.generation.as_deref() {
                    runtime_supervisor::verify_quiescent(&root, id)?;
                }
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await??;
    Ok(())
}

pub fn scope(workspace: &Path) -> Result<PathBuf> {
    let app = std::env::var("PROJECT_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "unknown-app".into());
    crate::runtime_kernel::RuntimeStore::resolve_root(workspace, &app)
}
/// None means authenticated worker or an existing owner handled by dispatch.
pub async fn supervise(args: &crate::RuntimeArgs, restart_on_exit: bool) -> Result<Option<i32>> {
    if args.attach {
        return Ok(None);
    }
    let root = scope(&args.workspace)?;
    if Worker::from_env(&root).await?.is_some() {
        return Ok(None);
    }
    let Some(owner) = Owner::try_acquire(&root)? else {
        return Ok(None);
    };
    let mut options = Options::new(std::env::args_os().skip(1).collect());
    options.binding = Some(runtime_supervisor::Binding {
        component: "app-cli".into(),
        resource: runtime_state_layout::resolve_project_origin(&args.workspace)?,
    });
    options.restart_on_exit = restart_on_exit;
    options.external_cleanup_args = Some(vec!["--app-cli-cleanup-engine".into()]);
    options.recovery_remove_env = [
        "APP_DEPLOY_URL",
        "APP_RELEASE_ID",
        "APP_DEPLOY_SHA256",
        "APP_DEPLOY_GENERATION_ID",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    // Interactive controls must not inherit long per-service shutdown budgets.
    // After 3s the guardian terminates the captured worker; owned command
    // guardians then stop their trees and publish cleanup receipts.
    options.policy.graceful_stop = std::time::Duration::from_secs(STOP_GRACE_SECONDS);
    options.policy.negotiate_shutdown_grace = false;
    let cancel = options.shutdown.clone();
    let signals = tokio::spawn(async move {
        tokio::select! { () = crate::supervisor::sigterm_watch() => {}, _ = tokio::signal::ctrl_c() => {} }
        cancel.cancel();
    });
    let result = owner.run(options).await;
    signals.abort();
    result.map(Some)
}
pub async fn control(args: &crate::config::OwnerArgs) -> Result<()> {
    let action = match args.action {
        crate::config::OwnerAction::Status => Action::Status,
        crate::config::OwnerAction::Recover => Action::Recover,
        crate::config::OwnerAction::Stop => Action::StopWork,
        crate::config::OwnerAction::Shutdown => Action::Shutdown,
    };
    let mut request = Request::new(action);
    if let Some(id) = &args.request_id {
        request.request_id = id.clone();
    }
    request.expected_generation = args.generation.clone();
    let root = scope(&args.workspace.workspace)?;
    let result = match runtime_supervisor::control(&root, request.clone()).await {
        Ok(result) => result,
        Err(error) if matches!(action, Action::Shutdown | Action::StopWork) => {
            match Owner::try_acquire(&root)? {
                Some(owner) => owner.stop_offline(&request).await?,
                None => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

pub struct ForegroundControl {
    pub cancellation: tokio_util::sync::CancellationToken,
}
#[async_trait::async_trait]
impl runtime_supervisor::WorkerControl for ForegroundControl {
    async fn probe(&self) -> Result<()> {
        anyhow::ensure!(
            !self.cancellation.is_cancelled(),
            "foreground execution is shutting down"
        );
        Ok(())
    }
    async fn shutdown(&self) -> Result<()> {
        self.cancellation.cancel();
        Ok(())
    }
}
