use super::*;

fn legacy_fixture(root: &Path, exited: bool) -> (Receipt, Vec<(PathBuf, Vec<u8>)>) {
    let instance = uuid::Uuid::new_v4().to_string();
    let supervisor = uuid::Uuid::new_v4().to_string();
    let receipt = Receipt {
        version: VERSION,
        instance_id: instance.clone(),
        token: "private-native-regression-token-at-least-32-bytes".into(),
        control_address: "127.0.0.1:1".into(),
        address: "127.0.0.1:60000".into(),
        phase: "Running".into(),
        supervisor_id: Some(supervisor.clone()),
        retirement_requested: false,
        launch_request_id: None,
    };
    write(root, &receipt).unwrap();
    let witness = root.join("supervisors").join(&supervisor);
    std::fs::create_dir_all(&witness).unwrap();
    if exited {
        // Publish exit only after an actual owned child was waited, mirroring
        // the old native root witness's meaning (no recorded PID signals).
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--list"])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(result.success());
        std::fs::write(
            witness.join("witness.json"),
            serde_json::to_vec(&serde_json::json!({
                "version":1,"supervisor_id":supervisor,"instance_id":instance,"phase":"OwnerExited"
            }))
            .unwrap(),
        )
        .unwrap();
    }
    let workers = root.join("work").join(&receipt.instance_id).join("workers");
    std::fs::create_dir_all(&workers).unwrap();
    let mut files = Vec::new();
    for (name, bytes) in [
        ("broken.json", b"{damaged old worker".as_slice()),
        (
            "future.json",
            br#"{"version":999,"phase":"Running","opaque":"keep"}"#.as_slice(),
        ),
        (
            "future-phase.json",
            br#"{"version":1,"phase":"FuturePhase","opaque":"keep"}"#.as_slice(),
        ),
    ] {
        let path = workers.join(name);
        std::fs::write(&path, bytes).unwrap();
        files.push((path, bytes.to_vec()));
    }
    (receipt, files)
}

#[test]
fn exact_native_owner_exit_preserves_damaged_worker_bytes_without_vetoing_recovery() {
    let root = tempfile::tempdir().unwrap();
    let _owner = runtime_supervisor::Owner::try_acquire(root.path())
        .unwrap()
        .unwrap();
    let (receipt, files) = legacy_fixture(root.path(), true);
    let result = recover_under_owner_lock(root.path(), Some(&receipt.instance_id)).unwrap();
    assert!(result.contains("Stopped"));
    assert_eq!(read(root.path()).unwrap().unwrap().phase, "Stopped");
    for (path, bytes) in files {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn missing_native_owner_exit_still_protects_damaged_worker_history() {
    let root = tempfile::tempdir().unwrap();
    let _owner = runtime_supervisor::Owner::try_acquire(root.path())
        .unwrap()
        .unwrap();
    let (receipt, files) = legacy_fixture(root.path(), false);
    let owner = std::fs::read(root.path().join("owner.json")).unwrap();
    assert!(recover_under_owner_lock(root.path(), Some(&receipt.instance_id)).is_err());
    assert_eq!(
        std::fs::read(root.path().join("owner.json")).unwrap(),
        owner
    );
    for (path, bytes) in files {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}
