//! Damaged generation metadata is not evidence of a live old execution. The
//! held generation/guardian locks and actual command exit checks settle it;
//! original bytes remain available and no business result is invented.
use super::*;

pub(super) fn read_or_recover(
    root: &Path,
    current: Option<&crate::domain::PhysicalDomain>,
) -> Result<Option<Generation>> {
    match generation(root) {
        Ok(value) => return Ok(Some(value)),
        Err(error) if error.downcast_ref::<std::io::Error>().is_some() => return Err(error),
        Err(error) => {
            tracing::warn!(path = %root.display(), %error, "recovering historical generation metadata")
        }
    }
    ensure!(
        std::fs::canonicalize(root)? == root,
        "historical execution range is outside owner root"
    );
    let id = root
        .file_name()
        .and_then(|name| name.to_str())
        .context("historical generation path missing")?;
    uuid::Uuid::parse_str(id).context("invalid historical execution directory")?;
    let original = std::fs::read(root.join("generation.json"))?;
    let raw = serde_json::from_slice::<serde_json::Value>(&original).ok();
    let old_domain = raw
        .as_ref()
        .and_then(|value| value.get("physical_domain"))
        .filter(|value| !value.is_null())
        .and_then(|value| {
            serde_json::from_value::<crate::domain::PhysicalDomain>(value.clone()).ok()
        });
    if matches!((old_domain.as_ref(), current), (Some(old), Some(now))
        if old.authority == now.authority && old.volume == now.volume && old.instance != now.instance)
    {
        // As for a valid record, another container's execution remains history;
        // local PID observations may never retire it or signal its processes.
        return Ok(None);
    }
    let _generation = lock(&root.join("generation.lock"))?;
    process_utils::command_authority::Gate::try_acquire(root)?.close()?;
    match process_utils::guardian::recover_with_scope_check(root)? {
        process_utils::guardian::RecoveryOutcome::Settled => {}
        process_utils::guardian::RecoveryOutcome::Stopping { detail } => {
            anyhow::bail!("historical command execution still draining: {detail}");
        }
    }
    process_utils::command_context::require_quiescent(&root.join("commands"))?;
    let backup = root.join(format!(
        "generation.history-{}.json",
        uuid::Uuid::new_v4().simple()
    ));
    let mut retained = tempfile::NamedTempFile::new_in(root)?;
    retained.write_all(&original)?;
    retained.as_file().sync_all()?;
    retained
        .persist(&backup)
        .map_err(|error| error.error)
        .context("preserve original historical generation bytes")?;
    let supervisor = raw
        .as_ref()
        .and_then(|value| value.get("supervisor"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| uuid::Uuid::parse_str(value).is_ok())
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let value = Generation {
        version: 1, id: id.to_owned(), supervisor,
        token: uuid::Uuid::new_v4().to_string(), intent: Intent::Stopped,
        phase: GenerationPhase::Revoked, worker_pid: None, exit_code: None,
        error: Some("Historical execution metadata was repaired after actual command quiescence; the original business result remains unknown".into()),
        physical_domain: current.cloned(), process_epoch: crate::epoch::current(),
    };
    save(&root.join("generation.json"), &value)?;
    tracing::warn!(backup = %backup.display(), "historical generation settled without replaying its commands");
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damaged_metadata_with_no_actual_execution_preserves_bytes_and_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory
            .path()
            .canonicalize()
            .unwrap()
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&root).unwrap();
        let bytes = b"{damaged generation";
        std::fs::write(root.join("generation.json"), bytes).unwrap();
        let repaired = read_or_recover(&root, None).unwrap().unwrap();
        assert_eq!(repaired.phase, GenerationPhase::Revoked);
        assert_eq!(repaired.exit_code, None);
        assert_eq!(repaired.worker_pid, None);
        let histories: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("generation.history-")
            })
            .collect();
        assert_eq!(histories.len(), 1);
        assert_eq!(std::fs::read(&histories[0]).unwrap(), bytes);
    }

    #[test]
    fn damaged_metadata_never_overrides_an_actual_generation_lock() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory
            .path()
            .canonicalize()
            .unwrap()
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&root).unwrap();
        let bytes = b"{damaged generation";
        std::fs::write(root.join("generation.json"), bytes).unwrap();
        let _owner = lock(&root.join("generation.lock")).unwrap();
        let error = read_or_recover(&root, None).err().unwrap();
        assert!(error.downcast_ref::<std::fs::TryLockError>().is_some());
        assert_eq!(std::fs::read(root.join("generation.json")).unwrap(), bytes);
    }
}
