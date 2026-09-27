//! app-cli adapter for the common local supervisor. Business journals remain
//! owned by the worker; the parent only records process-control intent.
use anyhow::Result;
use runtime_supervisor::{Action, Options, Owner, Request, Worker};
use std::path::{Path, PathBuf};

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
    options.recovery_remove_env = [
        "APP_DEPLOY_URL",
        "APP_RELEASE_ID",
        "APP_DEPLOY_SHA256",
        "APP_DEPLOY_GENERATION_ID",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    // Allow the existing graceful engine shutdown to finish before force.
    options.policy.graceful_stop = std::time::Duration::from_secs(30);
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
