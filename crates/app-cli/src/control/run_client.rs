//! Explicit run intent is a client operation; the existing serve process owns
//! management for its whole lifetime, including business failure and Stop.
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::{
    RuntimeArgs,
    owner_dispatch::{EnvironmentDeployment, ManagementReuse, OwnerDispatch},
};

const REQUEST_BUDGET: Duration = Duration::from_secs(660);

fn deployment_input() -> Result<Option<EnvironmentDeployment>> {
    if !crate::deploy::deploy_requested() {
        return Ok(None);
    }
    let request = crate::deploy::request_from_env()?;
    let require = |key| {
        std::env::var(key)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .with_context(|| format!("{key} is required for an explicit environment deployment"))
    };
    Ok(Some(EnvironmentDeployment {
        request,
        operation_id: require(shared_types::APP_DEPLOY_OPERATION_ID)?,
        generation: require(shared_types::APP_DEPLOY_GENERATION_ID)?,
    }))
}

fn start_management(args: &RuntimeArgs) -> Result<(Child, PathBuf)> {
    std::fs::create_dir_all(&args.log_dir).context("create owner bootstrap log directory")?;
    let log = args.log_dir.join(format!(
        "owner-bootstrap-{}.log",
        uuid::Uuid::new_v4().simple()
    ));
    let output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&log)
        .context("create independent owner bootstrap log")?;
    let mut command = Command::new(
        std::env::current_exe().context("locate current app-cli for management bootstrap")?,
    );
    command
        .args(["serve", "--control-only", "--workspace"])
        .arg(&args.workspace)
        .arg("--log-dir")
        .arg(&args.log_dir)
        .arg("--admin-addr")
        .arg(&args.admin_addr)
        .arg("--pingap-bin")
        .arg(&args.pingap_bin)
        .env_remove("APP_CLI_ATTACH")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output));
    // These are client intents, never automatic manager startup requests.
    // Keep the platform generation and every credential in the inherited env.
    for key in [
        "APP_DEPLOY_URL",
        "APP_RELEASE_ID",
        "APP_DEPLOY_OPERATION_ID",
        "APP_DEPLOY_SHA256",
    ] {
        command.env_remove(key);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Independent console-control group; no Job or numeric PID takeover.
        command.creation_flags(0x0000_0200);
    }
    let child = command.spawn().with_context(|| {
        format!(
            "start persistent management; diagnostics: {}",
            log.display()
        )
    })?;
    Ok((child, log))
}

pub async fn run(args: &RuntimeArgs) -> Result<()> {
    let args = args.for_management()?;
    let application = std::env::var("PROJECT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown-app".into());
    let state = crate::runtime_kernel::RuntimeStore::resolve_root(&args.workspace, &application)?;
    // Preserve invalid client input until management has been restored. Its
    // error must not destroy the owner's independent lifetime.
    let deployment = deployment_input();
    let deadline = tokio::time::Instant::now() + REQUEST_BUDGET;
    let bootstrap_deadline = std::cmp::min(
        deadline,
        tokio::time::Instant::now() + Duration::from_secs(45),
    );
    let mut launched: Option<(Child, PathBuf)> = None;
    for _ in 0..3 {
        if let Some(lock) = runtime_supervisor::Owner::try_acquire(&state)? {
            // Only a real released kernel lock authorizes this bootstrap. The
            // actual serve child competes for that same stable lock itself.
            drop(lock);
            launched = Some(start_management(&args)?);
        }
        let reuse = tokio::time::timeout_at(
            bootstrap_deadline,
            crate::owner_dispatch::reuse_management_owner(
                &args.admin_addr,
                &args.workspace,
                &state,
                &application,
            ),
        )
        .await;
        if let Some((child, log)) = launched.as_mut()
            && let Some(exit) = child
                .try_wait()
                .context("observe launched management process")?
            && !exit.success()
        {
            bail!(
                "management bootstrap exited with {exit}; diagnostics: {}",
                log.display()
            );
        }
        match reuse.context(
            "run management bootstrap deadline exceeded; no business request was submitted",
        )?? {
            ManagementReuse::NoOwner => continue,
            ManagementReuse::Ready => {}
        }
        match deployment
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?
        {
            Some(deployment) => {
                return crate::owner_dispatch::dispatch_environment_deployment(
                    &args,
                    &state,
                    &application,
                    deployment,
                    deadline,
                )
                .await;
            }
            None => {
                match tokio::time::timeout_at(deadline, crate::owner_dispatch::dispatch_to_owner(&args.admin_addr, &args.workspace, &state, &application)).await.context("run Source request deadline exceeded; query the printed original operation ID before retrying")?? {
                    OwnerDispatch::Terminal(view) => {
                        // Foreground run reports its own accepted Start result.
                        // Serve's handoff uses a different exit contract to
                        // avoid supervisor restart loops after supersession.
                        if view.state == shared_types::RuntimeOperationState::Cancelled {
                            bail!("Source start operation {} was cancelled: {}", view.operation_id, view.error_message.as_deref().unwrap_or("superseded by a newer request"));
                        }
                        return crate::owner_dispatch::describe_terminal(&view);
                    }
                    OwnerDispatch::NoOwner => continue,
                }
            }
        }
    }
    bail!(
        "management owner changed repeatedly during run bootstrap; no competing orchestration is permitted"
    )
}
