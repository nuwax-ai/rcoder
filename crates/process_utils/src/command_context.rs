//! Scoped cancellation and durable evidence for locally owned command workers.
//! Arguments and environment (which may contain credentials) are never persisted.
use std::{
    future::Future,
    io::Write,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkIdentity {
    pub task_id: Option<String>,
    pub app_id: Option<String>,
}

#[derive(Clone)]
pub struct CommandContext {
    pub identity: WorkIdentity,
    pub cancellation: CancellationToken,
    pub journal_root: Option<PathBuf>,
    pub cleanup_pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
tokio::task_local! { static CURRENT: CommandContext; }
impl CommandContext {
    pub async fn scope<F: Future>(self, future: F) -> F::Output {
        CURRENT.scope(self, future).await
    }
    /// Link local work to the original external owner operation before any
    /// network submission. This is a reference, never proof of external cleanup.
    pub fn record_external_operation(
        operation_id: &str,
        runtime_instance_id: &str,
        workspace_id: &str,
    ) -> std::io::Result<()> {
        let Some(context) = Self::current() else {
            return Ok(());
        };
        let Some(commands) = context.journal_root else {
            return Ok(());
        };
        let root = commands
            .parent()
            .ok_or_else(|| std::io::Error::other("work root missing"))?
            .join("external");
        create_durable_directory(&root)?;
        let name: String = operation_id
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if name.is_empty() || name.len() > 240 {
            return Err(std::io::Error::other("invalid external operation identity"));
        }
        let value = serde_json::json!({
            "version": 1, "phase": "ExternalReference",
            "identity": context.identity, "operation_id": operation_id,
            "runtime_instance_id": runtime_instance_id, "workspace_id": workspace_id
        });
        let path = root.join(format!("{name}.json"));
        match std::fs::read(&path) {
            Ok(bytes) => {
                if serde_json::from_slice::<serde_json::Value>(&bytes)? != value {
                    return Err(std::io::Error::other(
                        "external work identity changed; recovery required",
                    ));
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut temp = tempfile::NamedTempFile::new_in(&root)?;
        serde_json::to_writer(&mut temp, &value)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        // No replacement: a concurrent claimant must verify the same identity.
        match temp.persist_noclobber(&path) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path)?)? != value {
                    return Err(std::io::Error::other(
                        "external operation identity conflict",
                    ));
                }
            }
            Err(error) => return Err(error.error),
        }
        #[cfg(unix)]
        std::fs::File::open(root)?.sync_all()?;
        Ok(())
    }
    pub fn current() -> Option<Self> {
        CURRENT.try_with(Clone::clone).ok()
    }
}

pub struct CommandRecord {
    identity: WorkIdentity,
    path: Option<PathBuf>,
    pending: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    confirmed: std::sync::atomic::AtomicBool,
}
impl CommandRecord {
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub fn prepare() -> std::io::Result<Option<Self>> {
        let Some(context) = CommandContext::current() else {
            return Ok(None);
        };
        let mut record = match context.journal_root {
            Some(root) => Self::prepare_identified(root, context.identity.clone())?,
            None => Self {
                identity: context.identity,
                path: None,
                pending: None,
                confirmed: std::sync::atomic::AtomicBool::new(false),
            },
        };
        context
            .cleanup_pending
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        record.pending = Some(context.cleanup_pending);
        Ok(Some(record))
    }
    pub fn prepare_in(root: PathBuf) -> std::io::Result<Self> {
        Self::prepare_identified(root, WorkIdentity::default())
    }
    pub fn prepare_identified(root: PathBuf, identity: WorkIdentity) -> std::io::Result<Self> {
        create_durable_directory(&root)?;
        let record = Self {
            identity,
            path: Some(root.join(format!("{}.json", uuid::Uuid::new_v4()))),
            pending: None,
            confirmed: std::sync::atomic::AtomicBool::new(false),
        };
        record.write("SpawnPending", None)?;
        Ok(record)
    }
    pub fn running(&self, pid: Option<u32>) -> std::io::Result<()> {
        self.write("Running", pid)
    }
    pub fn quiescent(&self) -> std::io::Result<()> {
        self.write("Quiescent", None)?;
        if !self
            .confirmed
            .swap(true, std::sync::atomic::Ordering::AcqRel)
            && let Some(pending) = &self.pending
        {
            pending.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
        Ok(())
    }
    fn write(&self, phase: &str, pid: Option<u32>) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let root = path
            .parent()
            .ok_or_else(|| std::io::Error::other("command journal parent missing"))?;
        let mut temp = tempfile::NamedTempFile::new_in(root)?;
        serde_json::to_writer(
            &mut temp,
            &serde_json::json!({"version":1,"phase":phase,"diagnostic_pid":pid,"identity":self.identity}),
        )?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|e| e.error)?;
        #[cfg(unix)]
        std::fs::File::open(root)?.sync_all()?;
        Ok(())
    }
}
/// Create each missing directory and flush its parent before accepting work.
pub fn create_durable_directory(path: &Path) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => return Ok(()),
        Ok(_) => return Err(std::io::Error::other("journal path is not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| std::io::Error::other("journal path needs an existing parent"))?;
    create_durable_directory(parent)?;
    match std::fs::create_dir(path) {
        Ok(()) => {}
        Err(error)
            if error.kind() == std::io::ErrorKind::AlreadyExists
                && std::fs::metadata(path)?.is_dir() => {}
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn require_quiescent(root: &Path) -> std::io::Result<()> {
    match std::fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(std::io::Error::other("command journal is not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        if value["version"] != 1 || value["phase"] != "Quiescent" {
            return Err(std::io::Error::other(
                "owned command cleanup remains unconfirmed",
            ));
        }
    }
    Ok(())
}

/// Preserve the actual child/wrapper after an uncertain bounded stop. The
/// command counter remains nonzero until both tree and durable receipt settle.
pub fn retain_cleanup(
    mut child: Option<crate::guardian::OwnedChild>,
    record: Option<CommandRecord>,
) {
    tokio::spawn(async move {
        loop {
            if let Some(owned) = &mut child {
                if matches!(
                    owned.stop(std::time::Duration::ZERO).await,
                    crate::managed_tree::StopOutcome::Unconfirmed
                ) {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
                child.take();
            }
            if record.as_ref().is_none_or(|r| r.quiescent().is_ok()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    #[tokio::test]
    async fn command_and_external_receipts_keep_original_task_identity() {
        let dir = tempfile::tempdir().unwrap();
        let identity = WorkIdentity {
            task_id: Some("task-a".into()),
            app_id: Some("app-a".into()),
        };
        let context = CommandContext {
            identity: identity.clone(),
            cancellation: CancellationToken::new(),
            journal_root: Some(dir.path().join("commands")),
            cleanup_pending: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };
        context
            .clone()
            .scope(async {
                let record = CommandRecord::prepare().unwrap().unwrap();
                record.running(Some(42)).unwrap();
                let path = std::fs::read_dir(dir.path().join("commands"))
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                let value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                assert_eq!(value["identity"]["task_id"], "task-a");
                CommandContext::record_external_operation("op-a", "owner-a", "ws-a").unwrap();
                CommandContext::record_external_operation("op-a", "owner-a", "ws-a").unwrap();
                assert!(
                    CommandContext::record_external_operation("op-a", "owner-b", "ws-a").is_err()
                );
                record.quiescent().unwrap();
            })
            .await;
        let mut successor = context;
        successor.identity.task_id = Some("task-b".into());
        successor
            .scope(async {
                assert!(
                    CommandContext::record_external_operation("op-a", "owner-a", "ws-a").is_err()
                );
            })
            .await;
        let path = std::fs::read_dir(dir.path().join("external"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["identity"]["task_id"], "task-a");
        assert_eq!(value["runtime_instance_id"], "owner-a");
        assert_eq!(value["phase"], "ExternalReference");
    }
}
