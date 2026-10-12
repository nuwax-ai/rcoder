use super::*;

fn fixture(phase: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let work = directory.path().join(uuid::Uuid::new_v4().to_string());
    let root = work
        .join("guardians")
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&root).unwrap();
    save(
        &root,
        &Receipt {
            version: 2,
            id: root.file_name().unwrap().to_str().unwrap().into(),
            instance_id: work.file_name().unwrap().to_str().unwrap().into(),
            phase: phase.into(),
            command_record: None,
            command_digest: "protocol-fixture".into(),
            root_status: None,
            diagnostic_pid: None,
            resident_identity: None,
        },
    )
    .unwrap();
    (directory, work, root)
}

#[test]
fn settled_scope_observation_preserves_all_known_guardian_receipt_bytes() {
    for (phase, settled) in [
        ("Quiescent", true),
        ("Revoked", true),
        ("Running", false),
        ("Pending", false),
    ] {
        let (_directory, work, root) = fixture(phase);
        let original = std::fs::read(root.join("receipt.json")).unwrap();
        assert_eq!(is_scope_settled(&work).unwrap(), settled);
        assert_eq!(std::fs::read(root.join("receipt.json")).unwrap(), original);
    }
}

#[test]
fn settled_scope_observation_preserves_pending_commands_and_rejects_bad_schema() {
    let (_directory, work, _) = fixture("Quiescent");
    let command = crate::command_context::CommandRecord::prepare_in(work.join("commands")).unwrap();
    let path = command.path().unwrap();
    let pending = std::fs::read(path).unwrap();
    assert!(!is_scope_settled(&work).unwrap());
    assert_eq!(std::fs::read(path).unwrap(), pending);
    command.running(None).unwrap();
    assert!(!is_scope_settled(&work).unwrap());
    command.quiescent().unwrap();
    assert!(is_scope_settled(&work).unwrap());
    std::fs::write(path, br#"{"version":1,"phase":"Unknown"}"#).unwrap();
    assert!(
        format!("{:#}", is_scope_settled(&work).unwrap_err())
            .contains("unknown command cleanup record phase")
    );
    std::fs::write(path, b"{malformed").unwrap();
    assert!(is_scope_settled(&work).is_err());
}

#[test]
fn settled_scope_observation_keeps_real_lock_errors_and_rejects_foreign_guardian() {
    let (_directory, work, root) = fixture("Quiescent");
    let held = lock(&root).unwrap();
    let error = is_scope_settled(&work).unwrap_err();
    assert!(
        error
            .downcast_ref::<std::fs::TryLockError>()
            .is_some_and(|error| matches!(error, std::fs::TryLockError::WouldBlock))
    );
    drop(held);
    let mut receipt = read(&root).unwrap();
    receipt.instance_id = uuid::Uuid::new_v4().to_string();
    save(&root, &receipt).unwrap();
    assert!(
        format!("{:#}", is_scope_settled(&work).unwrap_err())
            .contains("guardian belongs to different instance")
    );
}

#[test]
fn settled_scope_observation_fails_concretely_for_unknown_guardian_or_non_directory() {
    let (_directory, work, root) = fixture("Quiescent");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("receipt.json")).unwrap()).unwrap();
    value["phase"] = "Unknown".into();
    std::fs::write(
        root.join("receipt.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    assert!(
        format!("{:#}", is_scope_settled(&work).unwrap_err()).contains("unknown guardian receipt")
    );
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("commands"), b"not a directory").unwrap();
    assert!(
        format!("{:#}", is_scope_settled(directory.path()).unwrap_err())
            .contains("command journal is not a directory")
    );
}
