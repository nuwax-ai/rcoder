use super::*;

#[test]
fn management_boot_consumption_is_stopped_atomically_and_preserved_by_new_intents() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = RuntimeStore::open_with_root(directory.path().join("state"), &workspace).unwrap();
    store.store_desired(DesiredState::Running, 5).unwrap();
    assert!(store.consume_management_boot("boot-test").unwrap());
    assert_eq!(store.load_desired().unwrap(), (DesiredState::Stopped, 6));
    assert!(!store.consume_management_boot("boot-test").unwrap());
    assert_eq!(store.load_desired().unwrap(), (DesiredState::Stopped, 6));
    store.store_desired(DesiredState::Running, 6).unwrap();
    assert!(!store.consume_management_boot("boot-test").unwrap());
    assert_eq!(store.load_desired().unwrap(), (DesiredState::Running, 6));
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store.desired_path()).unwrap()).unwrap();
    assert_eq!(saved["management_boot_id"], "boot-test");
    assert!(store.consume_management_boot("new-physical-boot").unwrap());
    assert_eq!(store.load_desired().unwrap(), (DesiredState::Stopped, 7));
}

#[test]
fn damaged_current_desired_is_preserved_then_a_new_source_intent_succeeds() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = RuntimeStore::open_with_root(directory.path().join("state"), &workspace).unwrap();
    let bytes = b"{corrupt original desired";
    std::fs::write(store.desired_path(), bytes).unwrap();
    assert_eq!(store.load_desired().unwrap(), (DesiredState::Stopped, 0));
    assert_eq!(std::fs::read(store.desired_path()).unwrap(), bytes);
    store.repair_desired_after_quiescence().unwrap();
    let saved: Vec<_> = std::fs::read_dir(store.root.join("operations-quarantine"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(saved.len(), 1);
    assert_eq!(std::fs::read(&saved[0]).unwrap(), bytes);
    store.store_desired(DesiredState::Running, 0).unwrap();
    assert_eq!(store.load_desired().unwrap(), (DesiredState::Running, 0));
    std::fs::remove_file(store.desired_path()).unwrap();
    std::fs::create_dir(store.desired_path()).unwrap();
    assert!(
        store.load_desired().is_err(),
        "filesystem I/O errors remain real errors"
    );
}

#[test]
fn readable_foreign_project_in_bare_legacy_state_is_never_imported_or_locked() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("our-project");
    std::fs::create_dir_all(&project).unwrap();
    let legacy = directory.path().join(STATE_DIR_NAME);
    std::fs::create_dir_all(&legacy).unwrap();
    let bytes = serde_json::to_vec(
        &serde_json::json!({"application_id":"foreign", "service_family":"userapp-dev",
        "source_root":directory.path().join("foreign-project")}),
    )
    .unwrap();
    std::fs::write(legacy.join("identity.json"), &bytes).unwrap();
    std::fs::write(legacy.join("token"), b"foreign-private-token").unwrap();
    std::fs::write(
        legacy.join("desired.json"),
        b"{\"desired\":\"running\",\"revision\":33}",
    )
    .unwrap();
    let root = directory.path().join("our-state");
    let store = RuntimeStore::open_with_root(root.clone(), &project).unwrap();
    store.reconcile_legacy_after_quiescence().unwrap();
    assert!(!root.join("token").exists());
    assert!(!root.join("identity.json").exists());
    assert_eq!(std::fs::read(legacy.join("identity.json")).unwrap(), bytes);
    assert_eq!(
        std::fs::read(legacy.join("token")).unwrap(),
        b"foreign-private-token"
    );
    assert!(
        crate::platform::owner_guard::OwnerGuard::try_acquire(&legacy)
            .unwrap()
            .is_some()
    );
}
