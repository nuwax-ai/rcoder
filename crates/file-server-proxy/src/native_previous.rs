//! Bootstrap may preserve a previous container's receipt without claiming exit.
use super::{Receipt, read, recover_under_owner_lock};
use std::{io::Write, path::Path};

/// A readable owner receipt is discovery, not the current worker's authority.
/// Never use malformed history to infer that an execution exited. Authorization
/// proves the current actual PID/leases; every older work range is checked too.
pub(super) fn preserve_opaque(
    root: &Path,
    worker: &runtime_supervisor::Worker,
    bytes: &[u8],
) -> Result<(), String> {
    runtime_supervisor::verify_live(root, worker.supervisor_id(), worker.generation())
        .map_err(|e| format!("verify current native worker: {e:#}"))?;
    let snapshot = runtime_supervisor::last_snapshot(root).map_err(|e| format!("{e:#}"))?;
    if snapshot.binding.component != "file-server-proxy"
        || snapshot.binding.resource != root
        || snapshot.supervisor_id != worker.supervisor_id()
        || snapshot.generation.as_deref() != Some(worker.generation())
    {
        return Err("current native worker authority changed before history preservation".into());
    }
    let admission = process_utils::command_authority::Gate::try_acquire(worker.work_root())
        .map_err(|e| format!("verify current native admission: {e:#}"))?;
    admission.require_open().map_err(|e| format!("{e:#}"))?;
    let mut leases = Vec::new();
    for entry in std::fs::read_dir(root.join("work")).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let work = entry.path();
        let file_type = entry.file_type().map_err(|e| e.to_string())?;
        // Ordinary historical notes are not execution ranges. Preserve them
        // without letting a report file prevent an authorized new worker.
        if file_type.is_file() {
            continue;
        }
        if !file_type.is_dir() || std::fs::canonicalize(&work).map_err(|e| e.to_string())? != work {
            return Err("native execution history is outside its owned work directory".into());
        }
        let name = entry.file_name();
        let id = name.to_string_lossy();
        let id = id.as_ref();
        if id == worker.generation() {
            continue;
        }
        if work
            .join("generation.json")
            .try_exists()
            .map_err(|e| e.to_string())?
        {
            uuid::Uuid::parse_str(id).map_err(|_| "native generation identity is invalid")?;
            // None is positively identified previous-container history. Its
            // original commands/results remain untouched, not declared exited.
            runtime_supervisor::verify_local_quiescent(root, id)
                .map_err(|e| format!("previous native execution cleanup unconfirmed: {e:#}"))?;
        } else {
            leases.extend(verify_legacy_range(root, &work, id)?);
        }
    }
    // Keep legacy leases through the durable copy. A stale command/writer may
    // not register in those ranges while their opaque discovery is preserved.
    let mut file = tempfile::NamedTempFile::new_in(root).map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    let archive = root.join(format!("owner.history-{}.json", uuid::Uuid::new_v4()));
    file.persist_noclobber(&archive)
        .map_err(|e| e.error.to_string())?;
    #[cfg(unix)]
    std::fs::File::open(root)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    if std::fs::read(&archive).map_err(|e| e.to_string())? != bytes
        || std::fs::read(root.join("owner.json")).map_err(|e| e.to_string())? != bytes
    {
        return Err("native owner history changed before new publication".into());
    }
    drop(leases);
    Ok(())
}

fn verify_legacy_range(root: &Path, work: &Path, id: &str) -> Result<Vec<std::fs::File>, String> {
    let mut leases = Vec::new();
    for name in [
        "generation.lock",
        "external-cleanup.lock",
        "command-admission.lock",
    ] {
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(work.join(name))
            .map_err(|e| e.to_string())?;
        file.try_lock()
            .map_err(|e| format!("previous native execution lease remains held: {e}"))?;
        leases.push(file);
    }
    let exited = legacy_owner_exited(root, id)?;
    if exited {
        match process_utils::guardian::recover_with_scope_check(work)
            .map_err(|e| format!("legacy command exit observation: {e:#}"))?
        {
            process_utils::guardian::RecoveryOutcome::Settled => {}
            process_utils::guardian::RecoveryOutcome::Stopping { detail } => return Err(detail),
        }
    } else if !process_utils::guardian::is_scope_settled(work)
        .map_err(|e| format!("legacy native command cleanup cannot be observed: {e:#}"))?
    {
        return Err("legacy native command exit remains unconfirmed".into());
    }
    if !exited {
        process_utils::command_context::require_quiescent(&work.join("workers"))
            .map_err(|e| format!("legacy native worker exit remains unconfirmed: {e}"))?;
    }
    Ok(leases)
}

fn legacy_owner_exited(root: &Path, instance: &str) -> Result<bool, String> {
    let supervisors = root.join("supervisors");
    if !supervisors.try_exists().map_err(|e| e.to_string())? {
        return Ok(false);
    }
    for entry in std::fs::read_dir(supervisors).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let Some(supervisor) = path.file_name().and_then(|id| id.to_str()) else {
            continue;
        };
        // Unknown old witness metadata is not itself a new fence. Only a
        // matching exact exit witness can authorize unfinished worker history.
        if crate::native_supervisor::verify_exited(root, supervisor, instance).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn reconcile(root: &Path, previous: &Receipt) -> Result<(), String> {
    let work = root.join("work").join(&previous.instance_id);
    if work
        .join("generation.json")
        .try_exists()
        .map_err(|e| e.to_string())?
    {
        let supervisor = previous
            .supervisor_id
            .as_deref()
            .ok_or("original owner supervisor missing; history cannot be classified")?;
        let local = runtime_supervisor::verify_local_quiescent_for_supervisor(
            root,
            &previous.instance_id,
            supervisor,
        )
        .map_err(|e| format!("classify previous owner: {e:#}"))?;
        return reconcile_observed(root, previous, local);
    }
    recover_under_owner_lock(root, Some(&previous.instance_id)).map(|_| ())
}

fn reconcile_observed(
    root: &Path,
    previous: &Receipt,
    local: Option<runtime_supervisor::Quiescence>,
) -> Result<(), String> {
    if local.is_some() {
        return recover_under_owner_lock(root, Some(&previous.instance_id)).map(|_| ());
    }
    let current = read(root)?.ok_or("original owner receipt missing")?;
    if current.instance_id != previous.instance_id
        || current.supervisor_id != previous.supervisor_id
    {
        return Err("previous owner changed before history preservation".into());
    }
    // Keep the original Running/Stopping phase. A new local worker may publish
    // owner.json afterwards; no foreign commands or workers are marked exited.
    let bytes = std::fs::read(root.join("owner.json")).map_err(|e| e.to_string())?;
    let archive = root.join(format!(
        "owner.previous-container-{}.json",
        previous.instance_id
    ));
    match std::fs::read(&archive) {
        Ok(saved) if saved == bytes => {}
        Ok(_) => return Err("previous-container owner archive changed".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut temporary = tempfile::NamedTempFile::new_in(root).map_err(|e| e.to_string())?;
            temporary.write_all(&bytes).map_err(|e| e.to_string())?;
            temporary.as_file().sync_all().map_err(|e| e.to_string())?;
            process_utils::atomic_file::persist(temporary, &archive).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            std::fs::File::open(root)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
        Err(error) => return Err(format!("read previous-container owner archive: {error}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn foreign_bootstrap_keeps_original_phase_and_never_recovers_its_workers() {
        let root = tempfile::tempdir().unwrap();
        let previous = Receipt {
            version: 1,
            instance_id: uuid::Uuid::new_v4().to_string(),
            token: "private-test-token-at-least-32-bytes".into(),
            control_address: "127.0.0.1:1".into(),
            address: "127.0.0.1:60000".into(),
            phase: "Running".into(),
            supervisor_id: Some(uuid::Uuid::new_v4().to_string()),
            retirement_requested: false,
            launch_request_id: None,
        };
        super::super::native_receipt::write(root.path(), &previous).unwrap();
        let original = std::fs::read(root.path().join("owner.json")).unwrap();
        let work = root
            .path()
            .join("work")
            .join(&previous.instance_id)
            .join("workers");
        std::fs::create_dir_all(&work).unwrap();
        let unfinished = work.join("original.json");
        std::fs::write(&unfinished, "unknown foreign worker must remain unchanged").unwrap();
        reconcile_observed(root.path(), &previous, None).unwrap();
        reconcile_observed(root.path(), &previous, None).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("owner.json")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read(root.path().join(format!(
                "owner.previous-container-{}.json",
                previous.instance_id
            )))
            .unwrap(),
            original
        );
        assert_eq!(
            std::fs::read_to_string(unfinished).unwrap(),
            "unknown foreign worker must remain unchanged"
        );
        assert!(
            crate::native_supervisor::verify_exited(
                root.path(),
                previous.supervisor_id.as_deref().unwrap(),
                &previous.instance_id
            )
            .is_err()
        );
        let mut foreign = previous;
        foreign.supervisor_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(reconcile_observed(root.path(), &foreign, None).is_err());
    }
}
