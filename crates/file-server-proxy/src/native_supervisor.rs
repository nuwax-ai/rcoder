//! Same-binary root-exit witness. It owns only the owner Child handle, not a
//! process group/Job that would include independently owned app-cli business.
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

const PROXY_COMPONENT: &str = "file-server-proxy";

enum StartOwner {
    Owned {
        owner: runtime_supervisor::Owner,
        remaining_initialization: Duration,
    },
    Reused(String),
}

async fn select_start_owner(
    root: &Path,
    policy: &runtime_supervisor::Policy,
) -> Result<StartOwner, String> {
    let deadline = tokio::time::Instant::now() + policy.initialization_timeout;
    if let Some(owner) =
        runtime_supervisor::Owner::try_acquire(root).map_err(|e| format!("{e:#}"))?
    {
        return acquired_owner(owner, deadline);
    }
    // try_acquire established that this directory exists. Resolve it once;
    // later retries must never follow a changed alias to another scope.
    let scope = root
        .canonicalize()
        .map_err(|error| format!("canonicalize native scope: {error}"))?;
    loop {
        if let Some(owner) =
            runtime_supervisor::Owner::try_acquire(&scope).map_err(|e| format!("{e:#}"))?
        {
            return acquired_owner(owner, deadline);
        }
        // An offline status reader also briefly holds owner.lock. Only an
        // authenticated current control response can prove a reusable owner;
        // the durable Stopped projection is never live launch evidence.
        let diagnostic = match tokio::time::timeout_at(
            deadline,
            crate::native_control::observe_live_owner(&scope),
        )
        .await
        {
            Ok(Ok(crate::native_control::LiveOwner::Supervised(snapshot))) => {
                crate::native_control::verify_supervisor_scope(&scope, &snapshot)?;
                match snapshot.phase {
                    runtime_supervisor::Phase::Ready => {
                        return crate::native_control::project_supervisor_status(&scope, snapshot)
                            .map(StartOwner::Reused);
                    }
                    runtime_supervisor::Phase::Reconciling
                    | runtime_supervisor::Phase::Starting
                    | runtime_supervisor::Phase::Stopping
                    | runtime_supervisor::Phase::CleanupPending
                    | runtime_supervisor::Phase::RecoveryRequired
                    | runtime_supervisor::Phase::Stopped => snapshot.diagnostic(),
                }
            }
            Ok(Ok(owner @ crate::native_control::LiveOwner::Legacy(_))) => {
                return owner.status(&scope).map(StartOwner::Reused);
            }
            Ok(Err(error)) => error,
            Err(_) => "Live native owner observation exceeded the initialization budget".into(),
        };
        // The observer may have released its read lock during the RPC. Retry
        // the real lock before sleeping, without changing any saved intent.
        if let Some(owner) =
            runtime_supervisor::Owner::try_acquire(&scope).map_err(|e| format!("{e:#}"))?
        {
            return acquired_owner(owner, deadline);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "Native owner startup remains unconfirmed within its initialization budget: {diagnostic}"
            ));
        }
        tokio::time::sleep(policy.probe_interval.min(remaining)).await;
    }
}

fn acquired_owner(
    owner: runtime_supervisor::Owner,
    deadline: tokio::time::Instant,
) -> Result<StartOwner, String> {
    let remaining_initialization = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining_initialization.is_zero() {
        return Err(
            "Native owner initialization budget exhausted before acquiring authority".into(),
        );
    }
    Ok(StartOwner::Owned {
        owner,
        remaining_initialization,
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Witness {
    version: u32,
    supervisor_id: String,
    instance_id: String,
    phase: String,
}
fn directory(root: &Path, id: &str) -> Result<PathBuf, String> {
    uuid::Uuid::parse_str(id).map_err(|e| format!("invalid supervisor identity: {e}"))?;
    Ok(root.join("supervisors").join(id))
}
fn read(root: &Path, id: &str, instance: &str) -> Result<Witness, String> {
    let path = directory(root, id)?;
    let value: Witness = serde_json::from_slice(
        &std::fs::read(path.join("witness.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    if value.version != 1 || value.supervisor_id != id || value.instance_id != instance {
        return Err("owner witness identity mismatch".into());
    }
    Ok(value)
}
pub fn verify_live(root: &Path, id: &str, instance: &str) -> Result<(), String> {
    uuid::Uuid::parse_str(instance).map_err(|e| format!("invalid execution identity: {e}"))?;
    if root
        .join("work")
        .join(instance)
        .join("generation.json")
        .try_exists()
        .map_err(|e| e.to_string())?
    {
        return runtime_supervisor::verify_live(root, id, instance).map_err(|e| format!("{e:#}"));
    }
    let value = read(root, id, instance)?;
    if !matches!(value.phase.as_str(), "SpawnPending" | "Running") {
        return Err("owner supervisor no longer authorizes launch".into());
    }
    let lock = File::options()
        .read(true)
        .write(true)
        .open(directory(root, id)?.join("witness.lock"))
        .map_err(|e| e.to_string())?;
    match lock.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => Ok(()),
        _ => Err("owner supervisor unavailable".into()),
    }
}
pub fn verify_exited(root: &Path, id: &str, instance: &str) -> Result<(), String> {
    uuid::Uuid::parse_str(instance).map_err(|e| format!("invalid execution identity: {e}"))?;
    if root
        .join("work")
        .join(instance)
        .join("generation.json")
        .try_exists()
        .map_err(|e| e.to_string())?
    {
        let receipt =
            runtime_supervisor::verify_quiescent(root, instance).map_err(|e| format!("{e:#}"))?;
        return if receipt.supervisor_id == id {
            Ok(())
        } else {
            Err("supervisor identity mismatch".into())
        };
    }
    let value = read(root, id, instance)?;
    if value.phase != "OwnerExited" {
        return Err("original owner exit unconfirmed; witness preserved".into());
    }
    Ok(())
}
pub async fn run(root: &Path, args: &[String]) -> Result<i32, String> {
    let mut options = runtime_supervisor::Options::new(args.iter().map(Into::into).collect());
    let (owner, remaining_initialization) = match select_start_owner(root, &options.policy).await? {
        StartOwner::Owned {
            owner,
            remaining_initialization,
        } => (owner, remaining_initialization),
        StartOwner::Reused(status) => {
            println!("{status}");
            return Ok(0);
        }
    };
    options.policy.initialization_timeout = remaining_initialization;
    options.binding = Some(runtime_supervisor::Binding {
        component: PROXY_COMPONENT.into(),
        resource: owner.scope_root().to_owned(),
    });
    options.policy.graceful_stop = file_server_proxy::GRACEFUL_STOP_BUDGET;
    let cancellation = options.shutdown.clone();
    let signal = tokio::spawn(async move {
        if let Err(error) = crate::shutdown_signal().await {
            eprintln!("native signal error: {error}");
        }
        cancellation.cancel();
    });
    let result = owner.run(options).await.map_err(|e| format!("{e:#}"));
    signal.abort();
    result
}

#[cfg(test)]
#[path = "native_supervisor_start_tests.rs"]
mod start_tests;
