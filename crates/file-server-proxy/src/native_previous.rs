//! Bootstrap may preserve a previous container's receipt without claiming exit.
use super::{Receipt, read, recover_under_owner_lock};
use std::{io::Write, path::Path};

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
