//! Same-binary root-exit witness. It owns only the owner Child handle, not a
//! process group/Job that would include independently owned app-cli business.
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

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
    let owner = runtime_supervisor::Owner::try_acquire(root).map_err(|e| format!("{e:#}"))?;
    let Some(owner) = owner else {
        println!(
            "{}",
            crate::native_control::control(root, "status", None).await?
        );
        return Ok(0);
    };
    let mut options = runtime_supervisor::Options::new(args.iter().map(Into::into).collect());
    options.binding = Some(runtime_supervisor::Binding {
        component: "file-server-proxy".into(),
        resource: std::fs::canonicalize(root).map_err(|e| e.to_string())?,
    });
    options.policy.graceful_stop = Duration::from_secs(30);
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
