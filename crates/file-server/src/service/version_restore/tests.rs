use super::*;
use std::io::Write as _;

fn fixture_zip(path: &Path) {
    let mut archive = zip::ZipWriter::new(File::create(path).unwrap());
    archive
        .start_file("src/app.js", zip::write::SimpleFileOptions::default())
        .unwrap();
    archive.write_all(b"INCOMING").unwrap();
    archive.finish().unwrap();
}
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/app.js"), b"CURRENT_EDIT").unwrap();
    std::fs::write(root.join("late-edit.txt"), b"LATE_UNBACKED_EDIT").unwrap();
    let zip = temp.path().join("version.zip");
    fixture_zip(&zip);
    (temp, root, zip)
}

#[test]
fn interrupted_landing_rolls_back_actual_originals_including_unbacked_edits() {
    let (_keep, root, source) = fixture();
    inject_landing_failure(&root);
    let error = run(&root, &source, &[], &[]).unwrap_err();
    assert!(
        error.to_string().contains("original files restored"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(root.join("src/app.js")).unwrap(),
        b"CURRENT_EDIT"
    );
    assert_eq!(
        std::fs::read(root.join("late-edit.txt")).unwrap(),
        b"LATE_UNBACKED_EDIT"
    );
    assert!(!root.join(receipt::ACTIVE).exists());
    run(&root, &source, &[], &[]).unwrap();
    assert_eq!(std::fs::read(root.join("src/app.js")).unwrap(), b"INCOMING");
}

#[test]
fn crash_after_physical_move_resumes_from_captured_plan_without_replaying_new_content() {
    let (_keep, root, source) = fixture();
    let (mut active, mut plan) = prepare(&root, &source, &[], &[]).unwrap();
    // Stop after a real move and durable progress, leaving the original backup.
    commit::apply_with_hook(&root, &mut active, &mut plan, |_| {
        Err(AppError::system("simulated executor exit"))
    })
    .unwrap_err();
    assert!(root.join(receipt::ACTIVE).exists());
    recover(&root).unwrap();
    assert_eq!(
        std::fs::read(root.join("src/app.js")).unwrap(),
        b"CURRENT_EDIT"
    );
    assert_eq!(
        std::fs::read(root.join("late-edit.txt")).unwrap(),
        b"LATE_UNBACKED_EDIT"
    );
    assert!(!root.join(receipt::ACTIVE).exists());
}

#[test]
fn invalid_receipt_directory_cannot_touch_an_outside_sentinel() {
    let (_keep, root, source) = fixture();
    let (mut active, _) = prepare(&root, &source, &[], &[]).unwrap();
    let outside = root.parent().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), b"KEEP").unwrap();
    active.directory = "../outside".into();
    receipt::write_json(&root, Path::new(receipt::ACTIVE), &active).unwrap();
    assert!(recover(&root).is_err());
    assert_eq!(std::fs::read(outside.join("sentinel")).unwrap(), b"KEEP");
    assert_eq!(
        std::fs::read(root.join("src/app.js")).unwrap(),
        b"CURRENT_EDIT"
    );
}

#[test]
fn inconsistent_committed_progress_never_cleans_saved_originals() {
    let (_keep, root, source) = fixture();
    let (mut active, mut plan) = prepare(&root, &source, &[], &[]).unwrap();
    commit::apply_with_hook(&root, &mut active, &mut plan, |_| {
        Err(AppError::system("interrupted"))
    })
    .unwrap_err();
    assert!(plan.cursor < plan.actions.len());
    let saved = root.join(&plan.actions[0].target);
    let saved_id = receipt::identity(&saved).unwrap();
    assert_eq!(saved_id, Some(plan.actions[0].identity.clone()));
    active.state = State::Committed;
    receipt::write_json(&root, Path::new(receipt::ACTIVE), &active).unwrap();
    assert!(recover(&root).is_err());
    assert_eq!(receipt::identity(&saved).unwrap(), saved_id);
    assert!(root.join(receipt::ACTIVE).exists());
}

fn assert_forged_terminal_keeps_originals(state: State, forged_cleanup: bool) {
    let (_keep, root, source) = fixture();
    let (mut active, mut plan) = prepare(&root, &source, &[], &[]).unwrap();
    commit::apply_with_hook(&root, &mut active, &mut plan, |_| {
        Err(AppError::system("executor disappeared after first move"))
    })
    .unwrap_err();
    let saved = root.join(&plan.actions[0].target);
    let identity = receipt::identity(&saved).unwrap();
    assert_eq!(identity, Some(plan.actions[0].identity.clone()));
    active.state = state;
    active.cursor = match state {
        State::Committed => plan.actions.len(),
        State::RollingBack => 0,
        State::Applying => panic!("terminal fixture required"),
    };
    if forged_cleanup {
        active.cleanup = Some(format!(
            "{}{}",
            receipt::PREFIX,
            uuid::Uuid::now_v7().simple()
        ));
    }
    receipt::write_json(&root, Path::new(receipt::ACTIVE), &active).unwrap();
    let result = recover(&root);
    assert_eq!(
        receipt::identity(&saved).unwrap(),
        identity,
        "fake terminal progress must not delete the only saved original"
    );
    assert!(
        result.is_err(),
        "incomplete physical outcome must reject terminal cleanup"
    );
    assert!(root.join(receipt::ACTIVE).exists());
}

#[test]
fn forged_committed_full_cursor_requires_the_actual_target_identity_graph() {
    assert_forged_terminal_keeps_originals(State::Committed, false);
}

#[test]
fn forged_rolled_back_zero_cursor_requires_the_actual_source_identity_graph() {
    assert_forged_terminal_keeps_originals(State::RollingBack, false);
}

#[test]
fn forged_cleanup_target_without_quarantine_never_authorizes_original_deletion() {
    assert_forged_terminal_keeps_originals(State::Committed, true);
    assert_forged_terminal_keeps_originals(State::RollingBack, true);
}

#[test]
fn cleanup_authorized_before_quarantine_resumes_after_verifying_terminal_graph() {
    let (_keep, root, source) = fixture();
    let (mut active, mut plan) = prepare(&root, &source, &[], &[]).unwrap();
    commit::apply(&root, &mut active, &mut plan).unwrap();
    active.cleanup = Some(format!(
        "{}{}",
        receipt::PREFIX,
        uuid::Uuid::now_v7().simple()
    ));
    receipt::write_json(&root, Path::new(receipt::ACTIVE), &active).unwrap();
    assert_eq!(
        receipt::identity(&root.join(&active.directory)).unwrap(),
        Some(active.identity.clone())
    );
    assert!(
        receipt::identity(&root.join(active.cleanup.as_deref().unwrap()))
            .unwrap()
            .is_none()
    );
    recover(&root).unwrap();
    assert_eq!(std::fs::read(root.join("src/app.js")).unwrap(), b"INCOMING");
    assert!(!root.join(receipt::ACTIVE).exists());
}

#[test]
fn authorized_partial_cleanup_continues_without_requiring_already_deleted_backups() {
    let (_keep, root, source) = fixture();
    let (mut active, mut plan) = prepare(&root, &source, &[], &[]).unwrap();
    commit::apply(&root, &mut active, &mut plan).unwrap();
    let result = commit::cleanup_with_hook(&root, &mut active, |directory| {
        let saved = directory.join("backup").join("0");
        if std::fs::symlink_metadata(&saved)?.is_dir() {
            std::fs::remove_dir_all(saved)?;
        } else {
            std::fs::remove_file(saved)?;
        }
        Err(AppError::system("cleanup executor disappeared"))
    });
    assert!(result.is_err());
    assert!(active.cleanup.is_some());
    recover(&root).unwrap();
    assert_eq!(std::fs::read(root.join("src/app.js")).unwrap(), b"INCOMING");
    assert!(!root.join(receipt::ACTIVE).exists());
}

#[test]
fn captured_snapshot_survives_an_uncooperative_source_path_replacement() {
    let (_keep, root, source) = fixture();
    let mut snapshot = tempfile::tempfile().unwrap();
    crate::service::zip::capture_snapshot(&source, &mut snapshot).unwrap();
    let replacement = source.with_extension("replacement");
    std::fs::write(&replacement, b"not a zip").unwrap();
    std::fs::rename(replacement, &source).unwrap();
    crate::service::zip::validate_open_file(snapshot.try_clone().unwrap()).unwrap();
    let extracted = root.join("captured");
    crate::service::zip::extract_open_file(snapshot, &extracted).unwrap();
    assert_eq!(
        std::fs::read(extracted.join("src/app.js")).unwrap(),
        b"INCOMING"
    );
}
