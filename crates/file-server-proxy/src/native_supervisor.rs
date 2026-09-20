//! Same-binary root-exit witness. It owns only the owner Child handle, not a
//! process group/Job that would include independently owned app-cli business.
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
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
fn write(root: &Path, value: &Witness) -> Result<(), String> {
    let mut file = tempfile::NamedTempFile::new_in(root).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut file, value).map_err(|e| e.to_string())?;
    file.flush()
        .and_then(|()| file.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    file.persist(root.join("witness.json"))
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    File::open(root)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
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
    let value = read(root, id, instance)?;
    if value.phase != "OwnerExited" {
        return Err("original owner exit unconfirmed; witness preserved".into());
    }
    Ok(())
}
pub async fn run(root: &Path, args: &[String]) -> Result<i32, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let instance = std::env::var("FILE_SERVER_PROXY_LAUNCH_ID")
        .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    uuid::Uuid::parse_str(&instance).map_err(|e| e.to_string())?;
    let path = directory(root, &id)?;
    process_utils::command_context::create_durable_directory(&path).map_err(|e| e.to_string())?;
    let lock = File::options()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path.join("witness.lock"))
        .map_err(|e| e.to_string())?;
    lock.try_lock().map_err(|e| e.to_string())?;
    let mut witness = Witness {
        version: 1,
        supervisor_id: id.clone(),
        instance_id: instance.clone(),
        phase: "SpawnPending".into(),
    };
    write(&path, &witness)?;
    let mut command =
        tokio::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command
        .arg("start")
        .arg("--native-owner")
        .args(args)
        .env("FILE_SERVER_PROXY_OWNER_SUPERVISOR", &id)
        .env("FILE_SERVER_PROXY_LAUNCH_ID", &instance)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawn native owner: {e}"))?;
    witness.phase = "Running".into();
    // A failed witness write must not abandon the actual owner child handle.
    let mut stopping = write(&path, &witness).is_err();
    let signal = crate::shutdown_signal();
    tokio::pin!(signal);
    let status = loop {
        tokio::select! {
            result = child.wait() => break result.map_err(|e| format!("owner exit unknown: {e}"))?,
            result = &mut signal, if !stopping => { if let Err(error) = result { eprintln!("native signal registration failed: {error}"); } stopping = true; }
            () = tokio::time::sleep(Duration::from_millis(200)), if stopping => {
                // Every automatic signal is bound to this launch, never the
                // successor receipt currently occupying the same scope.
                let _stop = crate::native_control::control(root,"stop",Some(&instance)).await;
            }
        }
    };
    witness.phase = "OwnerExited".into();
    loop {
        match write(&path, &witness) {
            Ok(()) => break,
            Err(error) => {
                eprintln!("native owner exit receipt pending: {error}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    Ok(status.code().unwrap_or(1))
}
